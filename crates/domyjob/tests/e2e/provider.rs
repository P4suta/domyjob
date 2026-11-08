use std::process::ExitCode;

pub(crate) fn sleeper() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let number = |index: usize| {
        arguments
            .get(index)
            .and_then(|text| text.parse::<u64>().ok())
    };
    let milliseconds = number(0).unwrap_or(0).min(120_000);
    crate::pause(milliseconds);
    println!("sleeper slept {milliseconds}ms");
    number(1)
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::SUCCESS, ExitCode::from)
}
