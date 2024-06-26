use std::str::FromStr;

use cyphernet::addr::PeerAddrParseError;
use cyphernet::ed25519::PublicKey;

use crate::RemoteHost;

#[derive(Subcommand, Copy, Clone, Eq, PartialEq, Hash, Debug, Display)]
#[display(lowercase)]
pub enum LocalCommand {
    Echo,
    Date,
}

#[derive(Debug, Display, From, Error)]
pub enum InvalidCommand {
    #[display("invalid command {0}")]
    Unrecognized(String),

    #[display(inner)]
    #[from]
    RemoteAddr(PeerAddrParseError<PublicKey>),
}

impl FromStr for LocalCommand {
    type Err = InvalidCommand;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "echo" => LocalCommand::Echo,
            "date" => LocalCommand::Date,
            _ => return Err(InvalidCommand::Unrecognized(s.to_owned())),
        })
    }
}

#[derive(Subcommand, Clone, Eq, PartialEq, Hash, Debug, Display)]
pub enum Command {
    #[display("{command}")]
    Execute { command: LocalCommand },
    #[display("{command}@{hop}")]
    Forward {
        hop: RemoteHost,
        command: LocalCommand,
    },
}

impl Command {
    pub const ECHO: Command = Command::Execute {
        command: LocalCommand::Echo,
    };
    pub const DATE: Command = Command::Execute {
        command: LocalCommand::Date,
    };
}

impl FromStr for Command {
    type Err = InvalidCommand;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once("@") {
            None => Ok(Command::Execute {
                command: LocalCommand::from_str(s)?,
            }),
            Some((command, hop)) => Ok(Command::Forward {
                hop: hop.parse()?,
                command: LocalCommand::from_str(command)?,
            }),
        }
    }
}
