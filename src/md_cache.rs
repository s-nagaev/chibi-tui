//! Content-keyed cache of rendered markdown lines.
//!
//! [`crate::markdown::render`] is expensive: pulldown-cmark parsing plus
//! syntect highlighting for every fenced code block, and the chat renderer
//! repaints the whole transcript every frame — so every finalized
//! (historical) message was re-highlighted ten times per second, which is
//! the dominant CPU cost with a rich transcript.
//!
//! The cache keys rendered lines by `(hash(markdown), Theme)`. Width is
//! deliberately NOT part of the key: `markdown::render` is
//! width-independent (code-panel and table widths derive from the content,
//! and paragraph wrapping happens downstream in
//! [`crate::ui::wrap_message_rows_full`]). A pure content key makes the
//! cache self-invalidating — no explicit invalidation, no message-identity
//! bookkeeping, no LRU: identical content hashes to an identical key, and
//! duplicate messages dedupe for free.
//!
//! The cache lives on [`crate::app::App`] as session-only state (never
//! persisted), not on `Chat`/`Message`: the renderer borrows the chat list
//! immutably for the whole render loop, so a per-chat cache could not be
//! mutated without interior mutability — and keeping it out of the message
//! types leaves the serde snapshot surface completely untouched.
//!
//! Streaming interplay: the actively-streaming pending row consults a
//! sibling cache (`App::stream_prefix_cache`, same mechanics) ONLY for its
//! completed-block prefix (`md[..stable anchor]`); the in-progress tail
//! renders as plain text. The finalized row renders once on the terminal
//! frame and is cached from then on.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::markdown::MdLine;
use crate::theme::Theme;

/// Cache key: content hash of the markdown source plus the theme it was
/// rendered with. A pure content key needs no invalidation machinery.
pub type CacheKey = (u64, Theme);

/// Maximum number of cached renders before the whole map is dropped. Both
/// caches are plain HashMaps (no ordering), so there is no LRU and no
/// "clear oldest": a full clear at the cap is the only sound bound.
const CACHE_CAP: usize = 512;

/// Content-keyed cache of markdown render results with an injectable
/// renderer. Production passes [`crate::markdown::render`]; tests count
/// calls through the injection instead of instrumenting the renderer.
pub struct MarkdownCache<F: Fn(&str, &Theme) -> Vec<MdLine>> {
    renderer: F,
    entries: HashMap<CacheKey, Vec<MdLine>>,
}

impl<F: Fn(&str, &Theme) -> Vec<MdLine>> MarkdownCache<F> {
    /// Build a cache around the given renderer.
    pub fn new(renderer: F) -> Self {
        Self {
            renderer,
            entries: HashMap::new(),
        }
    }

    /// Rendered lines for `md`, computed once per `(content, theme)` pair.
    /// On a miss the renderer runs and the result is stored; on a hit the
    /// cached lines are cloned (cheap `Line<'static>` clones, still far
    /// cheaper than re-parsing + re-highlighting) and returned.
    pub fn render(&mut self, md: &str, theme: &Theme) -> Vec<MdLine> {
        let key = (content_hash(md), *theme);
        if let Some(cached) = self.entries.get(&key).cloned() {
            return cached;
        }
        // Growth bound (plan D2): the streaming prefix cache sees a new key
        // per completed block, so the map must not grow unbounded. Plain
        // full clear at the cap — no LRU, no "clear oldest" (HashMaps have
        // no usable ordering).
        if self.entries.len() > CACHE_CAP {
            self.entries.clear();
        }
        let lines = (self.renderer)(md, theme);
        self.entries.insert(key, lines.clone());
        lines
    }

    /// Number of distinct `(content, theme)` pairs currently cached.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing is cached yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Stable-per-process content hash (SipHash via `DefaultHasher`; a hash
/// collision would serve the other entry's cached content — a wrong result —
/// but the probability is ~2⁻⁶⁴ and therefore negligible).
fn content_hash(md: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    md.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::history::{load_chats_from, save_chat_in};
    use crate::model::Message;
    use ratatui::style::Color;

    /// Render result marker: each CALL of the injectable renderer produces
    /// one line, so hit/miss is observable through the returned lines
    /// without touching `markdown.rs`.
    fn marker_lines(md: &str, suffix: &str) -> Vec<MdLine> {
        vec![MdLine::from(format!("{md}{suffix}"))]
    }

    #[test]
    fn repeated_content_renders_once_and_serves_cached_clones() {
        let calls = std::cell::Cell::new(0usize);
        let mut cache = MarkdownCache::new(|md: &str, _theme: &Theme| {
            calls.set(calls.get() + 1);
            marker_lines(md, ":r1")
        });
        let theme = Theme::tokyo_night();

        let first = cache.render("**a**", &theme);
        let second = cache.render("**a**", &theme);

        assert_eq!(calls.get(), 1, "the second identical request is a hit");
        assert_eq!(first.len(), second.len(), "hit returns the same shape");
        assert_eq!(
            first[0].spans[0].content.as_ref(),
            second[0].spans[0].content.as_ref(),
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn changed_content_renders_again_under_a_new_key() {
        let calls = std::cell::Cell::new(0usize);
        let mut cache = MarkdownCache::new(|md: &str, _theme: &Theme| {
            calls.set(calls.get() + 1);
            marker_lines(md, ":r1")
        });
        let theme = Theme::tokyo_night();

        let _ = cache.render("**a**", &theme);
        let changed = cache.render("**a** ", &theme);

        assert_eq!(calls.get(), 2, "different content must miss the cache");
        assert_eq!(changed[0].spans[0].content.as_ref(), "**a** :r1");
        assert_eq!(cache.len(), 2, "both entries stay live (no eviction)");
    }

    #[test]
    fn theme_change_rerenders_identical_content() {
        let calls = std::cell::Cell::new(0usize);
        let mut cache = MarkdownCache::new(|md: &str, _theme: &Theme| {
            calls.set(calls.get() + 1);
            marker_lines(md, ":r1")
        });
        let theme = Theme::tokyo_night();
        let mut alt = Theme::tokyo_night();
        alt.fg = Color::Rgb(1, 2, 3);

        let _ = cache.render("**a**", &theme);
        let _ = cache.render("**a**", &alt);
        assert_eq!(calls.get(), 2, "the theme is part of the key");

        let _ = cache.render("**a**", &alt);
        assert_eq!(calls.get(), 2, "same (content, theme) pair hits again");
        assert_eq!(cache.len(), 2);
    }

    /// Growth bound (plan D2): past [`CACHE_CAP`] the whole map clears —
    /// plain HashMaps have no ordering, so there is no LRU to build.
    #[test]
    fn cache_clears_at_the_cap_instead_of_growing_unbounded() {
        let mut cache = MarkdownCache::new(|md: &str, _theme: &Theme| marker_lines(md, ":r1"));
        let theme = Theme::tokyo_night();
        for i in 0..(CACHE_CAP as u32 + 5) {
            let _ = cache.render(&format!("md {i}"), &theme);
        }
        assert!(
            cache.len() <= CACHE_CAP,
            "cache must clear at the cap, len={}",
            cache.len()
        );
    }

    #[test]
    fn populated_cache_stays_outside_the_snapshot_surface() {
        // App-level storage (plan D1): the cache is session state on App,
        // never on Chat/Message, so populating it must not alter the
        // persisted history format of the chat it rendered.
        let tag = format!(
            "md-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(tag);
        std::fs::create_dir_all(&root).expect("temp root created");

        let mut app = App::new(vec![crate::app::Chat::new("cached")]);
        let md = "# Head\n\n```rust\nfn main() {}\n```";
        app.chats[0]
            .messages
            .push(Message::assistant(md.to_owned()));
        let theme = Theme::tokyo_night();
        let rendered = app.md_cache.render(md, &theme);
        assert!(!rendered.is_empty());
        assert!(!app.md_cache.is_empty(), "the render populated the cache");

        // Same contract for the streaming prefix cache (plan D2): it is
        // session state on App, never part of the persisted format.
        let streamed = app
            .stream_prefix_cache
            .render("# Head\n\nin progress tail", &theme);
        assert!(!streamed.is_empty());
        assert!(!app.stream_prefix_cache.is_empty());

        let path = save_chat_in(Some(&root), &app.chats[0]).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read snapshot");
        assert!(
            !raw.contains("md_cache"),
            "the cache must not leak into the snapshot format: {raw}"
        );

        let loaded = load_chats_from(Some(&root));
        assert_eq!(loaded.len(), 1, "round trip preserves the thread");
        assert_eq!(
            loaded[0].messages.len(),
            1,
            "round trip preserves the message count"
        );
        assert_eq!(
            loaded[0].messages[0].markdown, md,
            "round trip preserves the message content unchanged"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
