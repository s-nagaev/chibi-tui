use super::super::*;
use super::support::*;

//------------------------------------------------------------------------

//------------------------------------------------------------------------

/// The status bar must always carry the connection indicator.
#[test]
fn status_bar_shows_connection_indicator() {
    let mut app = App::new(mock::initial_chats());

    app.connection = Connection::Connected;
    let rows = render_grid(&mut app);
    let last = rows.last().unwrap();
    assert!(last.contains("connected"), "got {last:?}");
    assert!(!last.contains("disconnected"));

    app.connection = Connection::Connecting;
    assert!(render_grid(&mut app).last().unwrap().contains("connecting"));

    app.connection = Connection::Disconnected;
    let last = render_grid(&mut app).last().unwrap().clone();
    assert!(
        last.contains("disconnected") && last.contains("press R"),
        "got {last:?}"
    );
}

#[test]
fn connection_status_maps_states_to_labels() {
    let mut app = App::new(Vec::new());
    app.connection = Connection::Connected;
    let (label, _) = connection_status(&app);
    assert_eq!(label, "● connected");
    app.connection = Connection::Connecting;
    let (label, _) = connection_status(&app);
    assert_eq!(label, "● connecting…");
    app.connection = Connection::Disconnected;
    let (label, _) = connection_status(&app);
    assert_eq!(label, "● disconnected (press R)");
}
