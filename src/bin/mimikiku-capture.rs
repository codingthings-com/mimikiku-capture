use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

use mimikiku_capture::net::{self, NetEvent};
use mimikiku_capture::{audio, ui};

/// Stream system/loopback audio to a Mimikiku server and watch the live transcript.
///
/// Every option can also come from a MIMIKIKU_* environment variable or a `.env`
/// file in the current directory (see .env.example); flags win over both.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of channels (downmixed to mono before sending)
    #[arg(long, env = "MIMIKIKU_CHANNELS", default_value_t = 1)]
    channels: u16,

    /// Sample rate in Hz
    #[arg(long, env = "MIMIKIKU_RATE", default_value_t = 16000)]
    rate: u32,

    /// Chunk size (frames per buffer)
    #[arg(long, env = "MIMIKIKU_CHUNK", default_value_t = 2048)]
    chunk: u32,

    /// Append raw transcript JSON lines to this file
    #[arg(long, env = "MIMIKIKU_OUTPUT")]
    output: Option<PathBuf>,

    /// WebSocket URL
    #[arg(long, env = "MIMIKIKU_WS_URL", default_value = "ws://localhost:7777/listen")]
    ws_url: String,

    /// Personal API token (create one in the web app under Account → API tokens)
    #[arg(long, env = "MIMIKIKU_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// Keep the transcript local: the server won't save it to your account
    #[arg(long, env = "MIMIKIKU_NO_SAVE")]
    no_save: bool,

    /// Add recordings to this existing transcript (its id is in the web app URL, /#/t/<id>)
    #[arg(long, env = "MIMIKIKU_TRANSCRIPT", conflicts_with = "no_save")]
    transcript: Option<String>,

    /// Substring of the input device name to use (case-insensitive).
    /// Defaults to the first "monitor"/"pipewire" device, then the default input.
    #[arg(long, env = "MIMIKIKU_DEVICE")]
    device: Option<String>,

    /// Skip chunks whose RMS is at or below this level (0 sends everything non-silent).
    #[arg(long, env = "MIMIKIKU_SILENCE_THRESHOLD", default_value_t = 0.0)]
    silence_threshold: f32,

    /// Start listening immediately instead of waiting for the space key
    #[arg(long, env = "MIMIKIKU_AUTOSTART")]
    autostart: bool,

    /// Directory where the `s` hotkey saves transcripts
    #[arg(long, env = "MIMIKIKU_SAVE_DIR", default_value = ".")]
    save_dir: PathBuf,

    /// List input devices and exit
    #[arg(long)]
    list_devices: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Only ./.env: walking up to parent directories could pick up the server's .env.
    if let Err(e) = dotenvy::from_path(".env")
        && !e.not_found()
    {
        return Err(format!(".env: {e}").into());
    }
    let args = Args::parse();

    if args.list_devices {
        for name in audio::list_devices()? {
            println!("{name}");
        }
        return Ok(());
    }

    // Fail fast, in plain text, before taking over the terminal.
    let device = audio::pick_device(args.device.as_deref())?;
    let device_name = audio::device_name(&device);

    let shared = Arc::new(audio::Shared::default());
    shared.paused.store(!args.autostart, Ordering::Relaxed);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<NetEvent>();
    let (audio_tx, audio_rx) = mpsc::channel::<String>(32);

    let err_tx = event_tx.clone();
    let stream = audio::start(
        &device,
        audio::Capture {
            channels: args.channels,
            rate: args.rate,
            chunk: args.chunk,
            threshold: args.silence_threshold,
        },
        shared.clone(),
        audio_tx,
        move |e| {
            let _ = err_tx.send(NetEvent::Error(e));
        },
    )?;

    let target = net::Target {
        url: args.ws_url.clone(),
        token: args.token.clone(),
        save: !args.no_save,
        transcript: args.transcript.clone(),
    };
    let net_task = tokio::spawn(net::run(target, args.output, audio_rx, event_tx, shared.clone()));

    let mut app = ui::App::new(device_name, args.ws_url, args.autostart);
    // ratatui::init installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let result = run_ui(&mut terminal, &mut app, &shared, &mut event_rx, &args.save_dir).await;
    ratatui::restore();

    net_task.abort();
    drop(stream);
    result
}

async fn run_ui(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut ui::App,
    shared: &audio::Shared,
    events: &mut mpsc::UnboundedReceiver<NetEvent>,
    save_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    loop {
        let stats = ui::Stats {
            sent: shared.sent.load(Ordering::Relaxed),
            dropped: shared.dropped.load(Ordering::Relaxed),
        };
        terminal.draw(|f| app.draw(f, &stats))?;

        tokio::select! {
            _ = tick.tick() => app.tick(shared.rms(), shared.take_peak()),
            Some(ev) = events.recv() => match ev {
                NetEvent::Conn(c) => app.conn = c,
                NetEvent::ServerState(s) => app.set_server_state(s),
                NetEvent::Saving(id) => app.set_notice(format!("saving to your account (transcript {id})")),
                NetEvent::Segments(s) => app.push_segments(s),
                NetEvent::Error(e) => app.set_error(e),
                NetEvent::Fatal(e) => app.set_fatal(e),
            },
            Some(Ok(Event::Key(key))) = keys.next() => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if ctrl => break,
                    KeyCode::Char(' ') | KeyCode::Char('p') | KeyCode::Enter => {
                        let paused = app.toggle_capture();
                        shared.paused.store(paused, Ordering::Relaxed);
                    }
                    KeyCode::Char('c') => app.clear(),
                    KeyCode::Char('s') => match app.save(save_dir) {
                        Ok(path) => app.set_notice(format!("saved {}", path.display())),
                        Err(e) => app.set_error(format!("save failed: {e}")),
                    },
                    KeyCode::Char('t') => app.toggle_timestamps(),
                    KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
                    KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1),
                    KeyCode::PageUp => app.scroll_up(app.page()),
                    KeyCode::PageDown => app.scroll_down(app.page()),
                    KeyCode::Home | KeyCode::Char('g') => app.scroll_top(),
                    KeyCode::End | KeyCode::Char('G') => app.scroll_bottom(),
                    _ => {}
                }
            }
        }
    }
    Ok(())
}
