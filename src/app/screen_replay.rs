//! Replay a client's frames through an emulator and check the screen.
//!
//! Every other render test looks at what the app *composed*. These look at
//! what a terminal on the other end of the socket ends up *showing* after the
//! stream of deltas it was actually sent. The two come apart whenever the
//! app's record of a client's screen stops matching the screen, and that gap
//! is what visual corruption is: cells the diff never repaints because it
//! believes they are already right.
//!
//! The check is always the same shape: drive the app, replay client one's
//! frames, snapshot what it shows, then attach a second client — which is sent
//! a full repaint — and compare. A fresh terminal shows the truth, so anything
//! client one shows that client two does not is corruption.

use tokio::sync::mpsc;

use super::App;
use crate::backend::{PaneBackend, PaneId};
use crate::session::protocol::ServerToClient;
use crate::session::sink::OutputSink;
use tokio::time::Duration;

/// A terminal on the other end of a client socket.
struct Replay {
    parser: vt100::Parser,
    rx: mpsc::UnboundedReceiver<ServerToClient>,
}

impl Replay {
    fn new(cols: u16, rows: u16) -> (Self, mpsc::UnboundedSender<ServerToClient>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                parser: vt100::Parser::new(rows, cols, 0),
                rx,
            },
            tx,
        )
    }

    /// Apply every frame sent so far, as the client's write loop would.
    fn drain(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            if let ServerToClient::Frame(bytes) = message {
                self.parser.process(&bytes);
            }
        }
    }

    /// The visible screen as rows of `(char, fg, bg)`, which is what corruption
    /// shows up in.
    fn screen(&self) -> Vec<Vec<(String, vt100::Color, vt100::Color)>> {
        let (rows, cols) = self.parser.screen().size();
        (0..rows)
            .map(|row| {
                (0..cols)
                    .map(|col| {
                        self.parser.screen().cell(row, col).map_or_else(
                            || (String::new(), vt100::Color::Default, vt100::Color::Default),
                            |cell| (cell.contents(), cell.fgcolor(), cell.bgcolor()),
                        )
                    })
                    .collect()
            })
            .collect()
    }
}

/// Compare two replays row by row, so a failure names the row that differs.
fn assert_same_screen(seen: &Replay, truth: &Replay, what: &str) {
    let seen_rows = seen.screen();
    let truth_rows = truth.screen();

    assert_eq!(
        seen_rows.len(),
        truth_rows.len(),
        "{what}: the two terminals are different heights"
    );

    for (index, (left, right)) in seen_rows.iter().zip(truth_rows.iter()).enumerate() {
        assert_eq!(
            left,
            right,
            "{what}: row {index} is not what a fresh repaint shows\n  showing: {:?}\n  should be: {:?}",
            left.iter().map(|(ch, _, _)| ch.as_str()).collect::<String>(),
            right.iter().map(|(ch, _, _)| ch.as_str()).collect::<String>(),
        );
    }
}

struct NoBackend;

#[async_trait::async_trait]
impl PaneBackend for NoBackend {
    async fn spawn(&mut self, _cmd: crate::backend::PaneCommand) -> anyhow::Result<PaneId> {
        Ok(PaneId(99))
    }

    async fn write(&mut self, _id: PaneId, _data: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }

    async fn resize(&mut self, _id: PaneId, _cols: u16, _rows: u16) -> anyhow::Result<()> {
        Ok(())
    }

    async fn kill(&mut self, _id: PaneId) -> anyhow::Result<()> {
        Ok(())
    }
}

/// An app with one pane, rendering to clients only.
fn app(cols: u16, rows: u16) -> App {
    let mut app = App::with_backend_for_test(Box::new(NoBackend), cols, rows, PaneId(1));
    app.sink = OutputSink::Null;
    app
}

async fn tick(app: &mut App) {
    app.dirty = true;
    app.tick(Duration::from_millis(16))
        .await
        .expect("the frame renders");
}

/// Attach a fresh terminal and let it repaint: what it shows is the truth.
async fn truth_of(app: &mut App, id: u64, cols: u16, rows: u16) -> Replay {
    let (mut replay, frames) = Replay::new(cols, rows);
    app.attach_client(id, cols, rows, true, false, frames).await;
    tick(app).await;
    replay.drain();
    replay
}

fn feed(app: &mut App, pane: PaneId, bytes: &[u8]) {
    if let Some(pane) = app.pane_mut(pane) {
        pane.process(bytes);
    }
}

/// A terminal that resizes has scrambled its own contents — every terminal
/// reflows, and none of them tells the program how. The session may still
/// measure the same, though, when another client is the smaller one: then
/// nothing about the composed frame changes, the diff finds no cells to paint,
/// and the resized terminal keeps whatever the reflow left on it.
#[tokio::test]
async fn a_client_that_resizes_is_repainted_even_when_the_session_does_not_move() {
    let mut app = app(80, 24);
    let (mut watcher, frames) = Replay::new(80, 24);
    app.attach_client(1, 80, 24, true, false, frames).await;

    // A second, larger client: the session stays at the smaller one's size.
    let (mut resizer, frames) = Replay::new(100, 30);
    app.attach_client(2, 100, 30, true, false, frames).await;

    feed(&mut app, PaneId(1), b"hello from the pane");
    tick(&mut app).await;
    watcher.drain();
    resizer.drain();

    // The user drags the bigger window: its terminal reflows, and the session
    // is still measured by the 80x24 one.
    resizer.parser.set_size(28, 96);
    app.resize_client(2, 96, 28).await;
    tick(&mut app).await;
    resizer.drain();

    let truth = truth_of(&mut app, 3, 96, 28).await;
    assert_same_screen(&resizer, &truth, "after a resize the session did not follow");
}

/// The plain single-client resize, which the session does follow.
#[tokio::test]
async fn a_resized_client_shows_the_new_layout() {
    let mut app = app(80, 24);
    let (mut client, frames) = Replay::new(80, 24);
    app.attach_client(1, 80, 24, true, false, frames).await;

    feed(&mut app, PaneId(1), b"hello from the pane");
    tick(&mut app).await;
    client.drain();

    client.parser.set_size(30, 100);
    app.resize_client(1, 100, 30).await;
    tick(&mut app).await;
    client.drain();

    let truth = truth_of(&mut app, 2, 100, 30).await;
    assert_same_screen(&client, &truth, "after a resize");
}

/// The bug the screenshots were of: `ESC[2J` clears to the background colour
/// that happens to be set, and after any frame that is the status bar's. Every
/// cell a repaint then skipped for being blank on both sides kept it, so the
/// empty half of a pane came back as a block of status-bar colour.
#[tokio::test]
async fn a_repaint_leaves_no_status_bar_colour_behind_it() {
    let mut app = app(80, 24);
    let (mut client, frames) = Replay::new(80, 24);
    app.attach_client(1, 80, 24, true, false, frames).await;

    // One line of output at the top: everything under it is blank, and blank
    // is what the clear gets to colour in.
    feed(&mut app, PaneId(1), b"one line, then nothing");
    tick(&mut app).await;
    client.drain();

    app.execute(crate::command::Command::parse_str("refresh-client").expect("parses"))
        .await
        .expect("the refresh runs");
    tick(&mut app).await;
    client.drain();

    let truth = truth_of(&mut app, 2, 80, 24).await;
    assert_same_screen(&client, &truth, "after refresh-client");
}

/// A terminal bigger than the session has cells the frame never covers. They
/// are cleared on the way in, and the clear must leave them blank rather than
/// painted in whatever colour was set at the time.
#[tokio::test]
async fn the_margin_of_an_oversized_terminal_is_left_blank() {
    let mut app = app(100, 30);
    let (mut big, frames) = Replay::new(100, 30);
    app.attach_client(1, 100, 30, true, false, frames).await;
    feed(&mut app, PaneId(1), b"a big terminal, on its own for now");
    tick(&mut app).await;
    big.drain();

    // A smaller terminal joins, and the session shrinks to fit it. The big one
    // is repainted — with a frame that no longer reaches its last six rows,
    // and with the status bar's colours still set from the frame before.
    let (_small, frames) = Replay::new(80, 24);
    app.attach_client(2, 80, 24, true, false, frames).await;
    tick(&mut app).await;
    big.drain();

    let rows = big.screen();
    for (index, row) in rows.iter().enumerate().skip(24) {
        for (col, (ch, _, bg)) in row.iter().enumerate() {
            assert_eq!(
                (ch.as_str(), *bg),
                ("", vt100::Color::Default),
                "row {index} column {col} is outside the session and not blank"
            );
        }
    }
}

/// Ordinary work: output, a split, a window switch. Nothing here should ever
/// leave a cell behind, and if it does the screenshot is the bug report.
#[tokio::test]
async fn splitting_and_switching_leaves_nothing_behind() {
    let mut app = app(80, 24);
    let (mut client, frames) = Replay::new(80, 24);
    app.attach_client(1, 80, 24, true, false, frames).await;

    feed(&mut app, PaneId(1), b"\x1b[41mred background everywhere\x1b[0m\r\nsecond line\r\n");
    tick(&mut app).await;

    app.execute(crate::command::Command::parse_str("split-h").expect("parses"))
        .await
        .expect("the split runs");
    app.snap_workspace_tweens(app.current_workspace)
        .await
        .expect("tweens settle");
    tick(&mut app).await;

    // Window 2 has to exist before it can be switched to: `select-window` on an
    // empty window is rejected, and `execute` swallows a rejection, so without
    // this the switch below would quietly do nothing and the test would pass
    // while exercising none of what it names.
    app.execute_now_for_test(crate::command::Command::parse_str("new-window -d -t 2").expect("parses"))
        .await
        .expect("window 2 is created");
    tick(&mut app).await;

    app.execute_now_for_test(crate::command::Command::parse_str("select-window -t 2").expect("parses"))
        .await
        .expect("the switch runs");
    tick(&mut app).await;

    app.execute_now_for_test(crate::command::Command::parse_str("select-window -t 1").expect("parses"))
        .await
        .expect("the switch back runs");
    app.snap_workspace_tweens(app.current_workspace)
        .await
        .expect("tweens settle");
    tick(&mut app).await;
    client.drain();

    let truth = truth_of(&mut app, 2, 80, 24).await;
    assert_same_screen(&client, &truth, "after splitting and switching windows");
}
