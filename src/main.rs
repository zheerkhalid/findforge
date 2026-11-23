#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use findforge::ui::{self, Options};
use std::path::PathBuf;

const USAGE: &str = "FindForge

  FindForge.exe                      open the program
  FindForge.exe \"text\"               open the program with a search typed in
  FindForge.exe --tray               start hidden in the notification area
  FindForge.exe -s \"query\" [options] search from the command line and print full paths
        -n, --limit <count>          print at most this many results
        --sort name|path|size|modified|created|type   [--desc]
        --case  --whole-word  --path  --regex         matching options
        (the query may use content:text to look inside plain-text files)
        --csv                        print Name,Path,Size,Date Modified,Date Created,Type
        --count                      print only the number of results
        --refresh                    rescan the drives before searching
        (exit code 0 = found, 1 = nothing found, 2 = error)
  FindForge.exe --demo               show an invented index; nothing on this PC is read
  FindForge.exe --screenshot <png>   render the window to a PNG and exit
        [--view <name>] [--light]    (the view can also be set with FINDFORGE_VIEW)
  FindForge.exe --selftest           index a throwaway temp folder, change files in it and
                                     check that searches, live updates and saving all work
  FindForge.exe --bench [drive]      index (all drives, or one such as C:), then print the
                                     index time, memory per entry and query latencies";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let console = has("--help")
        || has("-h")
        || has("/?")
        || has("--selftest")
        || has("--bench")
        || has("-s")
        || has("--search");
    if console {
        findforge::sys::attach_console();
    }

    if has("--help") || has("-h") || has("/?") {
        println!("{USAGE}");
        return Ok(());
    }
    if has("--selftest") {
        match findforge::selftest::run() {
            Ok(log) => {
                for line in log {
                    println!("{line}");
                }
                println!("SELFTEST PASSED");
                std::process::exit(0);
            }
            Err(e) => {
                println!("{e}");
                println!("SELFTEST FAILED");
                std::process::exit(1);
            }
        }
    }
    if has("--bench") {
        let drive = value("--bench").filter(|d| !d.starts_with("--"));
        std::process::exit(findforge::selftest::bench(drive.as_deref()));
    }
    match findforge::cli::parse_args(&args) {
        Ok(Some(search)) => std::process::exit(findforge::cli::run(&search)),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    }

    // The real program (not a demo or a screenshot) runs once per user.
    let real = !has("--demo") && value("--screenshot").is_none();
    let mut _running = None;
    if real {
        let settings = findforge::settings::Settings::load(&findforge::sys::app_dir());
        if settings.always_admin && !findforge::sys::is_elevated() && !has("--restarted") {
            // Start again with administrator rights; if the prompt is declined,
            // carry on with standard rights.
            let passed: Vec<String> = args
                .iter()
                .map(|a| {
                    if a.contains(' ') {
                        format!("\"{a}\"")
                    } else {
                        a.clone()
                    }
                })
                .chain(std::iter::once("--restarted".to_string()))
                .collect();
            if findforge::sys::restart_as_admin(&passed.join(" ")).is_ok() {
                return Ok(());
            }
        }
        let wait = has("--restarted").then(|| std::time::Duration::from_secs(8));
        match findforge::desktop::single_instance(wait) {
            Some(guard) => _running = Some(guard),
            // Another copy is running and has been asked to show itself.
            None => return Ok(()),
        }
    }

    let known_flags = ["--demo", "--light", "--tray", "--restarted"];
    let known_values = ["--screenshot", "--view"];
    let mut initial_query = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if known_values.contains(&a.as_str()) {
            i += 1;
        } else if !known_flags.contains(&a.as_str()) && !a.starts_with("--") {
            initial_query = Some(a.clone());
        }
        i += 1;
    }
    let options = Options {
        demo: has("--demo"),
        screenshot: value("--screenshot").map(PathBuf::from),
        view: value("--view").or_else(|| std::env::var("FINDFORGE_VIEW").ok()),
        light: has("--light"),
        start_hidden: has("--tray"),
        initial_query,
    };
    ui::run(options, window_icon())
}

fn window_icon() -> egui::IconData {
    let bytes = include_bytes!(concat!(env!("OUT_DIR"), "/icon-256.png"));
    match image::load_from_memory(bytes) {
        Ok(img) => {
            let rgba = img.into_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(_) => egui::IconData::default(),
    }
}
