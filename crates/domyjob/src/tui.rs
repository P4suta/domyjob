use crate::output::Output;

pub(crate) mod frame;
pub(crate) mod model;
mod output;
mod render;
mod runtime;
mod worker;

#[derive(Debug, thiserror::Error)]
pub(crate) enum TuiError {
    #[error(
        "the dashboard needs terminal input and output; use an explicit command or --help when scripting"
    )]
    NotTerminal,
    #[error(transparent)]
    Terminal(#[from] crate::platform::terminal::SessionFailure),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub(crate) fn run(actor: Option<&str>, output: Output) -> Result<(), TuiError> {
    runtime::run(actor, output.into_stream())
}
