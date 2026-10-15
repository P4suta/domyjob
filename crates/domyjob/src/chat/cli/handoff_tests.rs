use crate::chat::ops::{
    self,
    handoff::tests::{fixture, read_args},
};
use crate::chat::store::handoff;

use super::{CliError, deliver_outcome};

#[test]
fn continuing_handoff_discovery_is_a_dedicated_scoped_command() {
    #[derive(clap::Parser)]
    struct Fixture {
        #[command(flatten)]
        chat: super::ChatArgs,
    }
    let parsed = <Fixture as clap::Parser>::try_parse_from([
        "chat",
        "--as",
        "continuing",
        "handoff",
        "list",
        "--project",
        "owner/project",
        "--scope",
        "issue-47",
    ])
    .unwrap();
    assert!(matches!(
        parsed.chat.command,
        super::ChatCommand::Handoff {
            command: super::HandoffCommand::List(_)
        }
    ));
}

#[test]
fn failed_handoff_output_does_not_publish_an_actual_reader_observation() {
    let fixture = fixture();
    let outcome = ops::read_handoff(&fixture.continuing, &read_args(&fixture.descriptor)).unwrap();
    let result = deliver_outcome(&fixture.continuing, &outcome, |_value, _text| {
        Err(CliError::Io(std::io::ErrorKind::BrokenPipe.into()))
    });
    assert!(
        matches!(result, Err(CliError::Io(error)) if error.kind()==std::io::ErrorKind::BrokenPipe)
    );
    assert!(
        !fixture
            .continuing
            .store
            .read(|read| handoff::observed(
                read,
                fixture.continuing.me().unwrap(),
                fixture.descriptor.id()
            ))
            .unwrap()
    );
    deliver_outcome(&fixture.continuing, &outcome, |_value, _text| Ok(())).unwrap();
    assert!(
        fixture
            .continuing
            .store
            .read(|read| handoff::observed(
                read,
                fixture.continuing.me().unwrap(),
                fixture.descriptor.id()
            ))
            .unwrap()
    );
}
