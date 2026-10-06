mod audio;
mod convert;
mod deepgram;
mod devtools;
mod logging;
mod messaging;
mod protocol;
mod screen;
mod state;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        // Chrome launches native hosts with the caller origin as the first argument.
        Some(origin) if origin.starts_with("chrome-extension://") => run_native_host(origin),
        Some("--meter") => devtools::run_meter(parse_seconds(&args)),
        Some("--pcm-test") => devtools::run_pcm_test(parse_seconds(&args)),
        Some("--transcribe-stderr") => devtools::run_transcribe_stderr(parse_seconds(&args)),
        Some("--capture-screen") => run_capture_screen(),
        Some("--help") | Some("-h") | None => {
            print_usage();
            0
        }
        Some(other) => {
            eprintln!("Unknown argument: {other}\n");
            print_usage();
            2
        }
    };
    std::process::exit(code);
}

fn print_usage() {
    eprintln!(
        "System Audio Companion\n\n\
         Chrome starts this program automatically through Native Messaging.\n\n\
         Developer modes:\n  \
         system-audio-companion.exe --meter [--seconds N]      Show a live system-audio level meter\n  \
         system-audio-companion.exe --pcm-test [--seconds N]   Show PCM16 mono conversion statistics\n  \
         system-audio-companion.exe --transcribe-stderr [--seconds N]\n      \
         Stream system audio to Deepgram and print transcripts (needs DEEPGRAM_API_KEY)\n  \
         system-audio-companion.exe --capture-screen        Save a cropped primary-display screenshot\n"
    );
}

fn run_capture_screen() -> i32 {
    match screen::capture_and_save() {
        Ok(o) => {
            let saved = o.path.map(|p| p.display().to_string());
            eprintln!(
                "Captured {}x{}; saved: {}; copied to clipboard: {}",
                o.width,
                o.height,
                saved.as_deref().unwrap_or("no"),
                o.copied
            );
            0
        }
        Err(e) => {
            eprintln!("Screen capture failed: {e}");
            1
        }
    }
}

fn parse_seconds(args: &[String]) -> Option<u64> {
    args.iter()
        .position(|a| a == "--seconds")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
}

fn run_native_host(origin: &str) -> i32 {
    let log_path = logging::init(true);
    std::panic::set_hook(Box::new(|info| {
        log_error!("Internal error: {info}");
    }));
    log_info!("Companion started (caller {origin})");
    if let Some(p) = log_path {
        log_info!("Log file: {}", p.display());
    }

    let (out_tx, out_rx) = tokio::sync::mpsc::unbounded_channel();
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel();
    let writer = match messaging::spawn_stdout_writer(out_rx) {
        Ok(w) => w,
        Err(e) => {
            log_error!("Could not start stdout writer: {e}");
            return 1;
        }
    };
    if let Err(e) = messaging::spawn_stdin_reader(in_tx) {
        log_error!("Could not start stdin reader: {e}");
        return 1;
    }
    log_info!("Native Messaging initialized");

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log_error!("Could not create runtime: {e}");
            return 1;
        }
    };
    runtime.block_on(state::Controller::new(out_tx).run(in_rx));
    let _ = writer.join();
    log_info!("Companion exiting");
    0
}
