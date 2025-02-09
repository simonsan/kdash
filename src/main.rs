mod app;
mod banner;
mod cli;
mod event;
mod handlers;
mod network;
mod ui;

use crate::event::Key;
use app::App;
use cli::Cli;
use network::{get_client, IoEvent, Network};

use anyhow::Result;
use backtrace::Backtrace;
use crossterm::{
  event::{DisableMouseCapture, EnableMouseCapture},
  execute,
  style::Print,
  terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
  io::{self, stdout, Stdout},
  panic::{self, PanicInfo},
  sync::{mpsc, Arc},
};
use tokio::sync::Mutex;
use tui::{
  backend::{Backend, CrosstermBackend},
  Terminal,
};

// shutdown the CLI and show terminal
fn shutdown(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
  disable_raw_mode()?;
  execute!(
    terminal.backend_mut(),
    LeaveAlternateScreen,
    DisableMouseCapture
  )?;
  terminal.show_cursor()?;
  Ok(())
}

fn panic_hook(info: &PanicInfo<'_>) {
  if cfg!(debug_assertions) {
    let location = info.location().unwrap();

    let msg = match info.payload().downcast_ref::<&'static str>() {
      Some(s) => *s,
      None => match info.payload().downcast_ref::<String>() {
        Some(s) => &s[..],
        None => "Box<Any>",
      },
    };

    let stacktrace: String = format!("{:?}", Backtrace::new()).replace('\n', "\n\r");

    disable_raw_mode().unwrap();
    execute!(
      io::stdout(),
      LeaveAlternateScreen,
      Print(format!(
        "thread '<unnamed>' panicked at '{}', {}\n\r{}",
        msg, location, stacktrace
      )),
      DisableMouseCapture
    )
    .unwrap();
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  panic::set_hook(Box::new(|info| {
    panic_hook(info);
  }));

  let mut cli: Cli = Cli::new();
  let clap_app = cli.get_clap_app();
  let matches = clap_app.get_matches();

  if let Some(tick_rate) = matches
    .value_of("tick-rate")
    .and_then(|tick_rate| tick_rate.parse().ok())
  {
    if tick_rate >= 1000 {
      panic!("Tick rate must be below 1000");
    } else {
      cli.tick_rate = tick_rate;
    }
  }

  if let Some(poll_rate) = matches
    .value_of("poll-rate")
    .and_then(|poll_rate| poll_rate.parse().ok())
  {
    if (poll_rate % cli.tick_rate) > 0u64 {
      panic!("Poll rate must be multiple of tick-rate");
    } else {
      cli.poll_rate = poll_rate;
    }
  }

  let (sync_io_tx, sync_io_rx) = mpsc::channel::<IoEvent>();

  // Initialize app state
  let app = Arc::new(Mutex::new(App::new(
    sync_io_tx,
    cli.enhanced_graphics,
    cli.poll_rate / cli.tick_rate,
  )));

  let cloned_app = Arc::clone(&app);

  // Launch network thread
  std::thread::spawn(move || {
    start_tokio(sync_io_rx, &app);
  });
  // Launch the UI (async)
  // The UI must run in the "main" thread
  start_ui(cli, &cloned_app).await?;

  Ok(())
}

#[tokio::main]
async fn start_tokio<'a>(io_rx: mpsc::Receiver<IoEvent>, app: &Arc<Mutex<App>>) {
  match get_client().await {
    Ok(client) => {
      let mut network = Network::new(client, app);

      while let Ok(io_event) = io_rx.recv() {
        network.handle_network_event(io_event).await;
      }
    }
    Err(e) => panic!("Unable to obtain Kubernetes client {}", e),
  }
}

async fn start_ui(cli: Cli, app: &Arc<Mutex<App>>) -> Result<()> {
  // Terminal initialization
  let mut stdout = stdout();
  execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
  // see https://docs.rs/crossterm/0.17.7/crossterm/terminal/#raw-mode
  enable_raw_mode()?;

  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend)?;
  terminal.hide_cursor()?;
  terminal.clear()?;

  let events = event::Events::new(cli.tick_rate);
  let mut is_first_render = true;

  loop {
    let mut app = app.lock().await;
    // Get the size of the screen on each loop to account for resize event
    if let Ok(size) = terminal.backend().size() {
      // Reset the help menu if the terminal was resized
      if app.refresh || app.size != size {
        app.help_menu_max_lines = 0;
        app.help_menu_offset = 0;
        app.help_menu_page = 0;

        app.size = size;

        // Based on the size of the terminal, adjust how many lines are
        // displayed in the help menu
        if app.size.height > 8 {
          app.help_menu_max_lines = (app.size.height as u32) - 8;
        } else {
          app.help_menu_max_lines = 0;
        }
      }
    };

    // draw the UI layout
    terminal.draw(|f| ui::draw(f, &mut app))?;

    // handle key vents
    match events.next()? {
      event::Event::Input(key) => {
        // handle CTRL + C
        if key == Key::Ctrl('c') {
          break;
        }
        // handle all other keys
        handlers::handle_app(key, &mut app)
      }
      event::Event::Tick => {
        app.on_tick(is_first_render);
      }
    }

    is_first_render = false;

    if app.should_quit {
      break;
    }
  }

  terminal.show_cursor()?;
  shutdown(terminal)?;

  Ok(())
}
