use std::{
    path::PathBuf,
    thread,
    time::Duration,
};

use log4rs::{
    Config,
    append::{
        console::ConsoleAppender,
        rolling_file::{
            RollingFileAppender,
            policy::compound::{
                CompoundPolicy, roll::fixed_window::FixedWindowRoller,
                trigger::onstartup::OnStartUpTrigger,
            },
        },
    },
    config::{Appender, Logger, Root},
    encode::pattern::PatternEncoder,
};

const DISABLED_MODULES: &[&str] = &[
    "wgpu_core",
    "wgpu_hal",
    "naga",
    "cosmic_text",
    "iced_wgpu",
    "iced_winit",
    "iced_beacon",
];

use crate::{DEBUG_MODE, root_dir};

pub fn log_dir() -> anyhow::Result<PathBuf> {
    Ok(root_dir()?.join("logs"))
}

pub fn setup() -> anyhow::Result<()> {
    const LEVEL_FILTER: log::LevelFilter = if DEBUG_MODE {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    let console_appender = Appender::builder().build(
        "console",
        Box::new(
            ConsoleAppender::builder()
                .encoder(Box::new(PatternEncoder::new("[{l}] {t} - {m}{n}")))
                .build(),
        ),
    );

    let log_file_path = log_dir()?.join("winri.log");
    let log_archive_file_pattern = log_dir()?.join("archive/winri-{}.log");

    let file_appender = Appender::builder().build(
        "file",
        Box::new(
            RollingFileAppender::builder()
                .encoder(Box::new(PatternEncoder::new("{d} [{l}] {t} - {m}{n}")))
                .build(
                    log_file_path,
                    Box::new(CompoundPolicy::new(
                        Box::new(OnStartUpTrigger::new(0)),
                        Box::new(
                            FixedWindowRoller::builder()
                                .build(&log_archive_file_pattern.display().to_string(), 20)?,
                        ),
                    )),
                )?,
        ),
    );

    let disabled_loggers = DISABLED_MODULES
        .iter()
        .map(|&module| Logger::builder().build(module, log::LevelFilter::Off));

    log4rs::init_config(
        Config::builder()
            .appenders([console_appender, file_appender])
            .loggers(disabled_loggers)
            .build(
                Root::builder()
                    .appenders(["console", "file"])
                    .build(LEVEL_FILTER),
            )?,
    )?;

    // log4rs's RollingFileAppender wraps the underlying file in a
    // BufWriter that only flushes on its own Drop. If winri is killed
    // (Task Manager, Stop-Process -Force, BSOD) the destructor never
    // runs and the entire tail of the log is lost — including the
    // diagnostic lines we'd want most for post-mortem. We also can't
    // tail the log in real time during a session.
    //
    // Pump explicit flushes from a daemon thread instead. 500ms is
    // imperceptible for diagnostics and the syscall cost is trivial.
    thread::Builder::new()
        .name("log-flush-pump".into())
        .spawn(|| {
            loop {
                thread::sleep(Duration::from_millis(500));
                log::logger().flush();
            }
        })
        .ok();

    Ok(())
}
