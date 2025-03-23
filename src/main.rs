use log::{debug, error};
use std::io::{Error, ErrorKind};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{Sender, channel};
use std::{fs, thread};
use stdext::function_name;

mod app;
mod events;
mod platform;
mod process;
mod skylight;
mod util;
mod windows;

use events::{Event, EventHandler};
use platform::PlatformCallbacks;
use skylight::{SLSGetSpaceManagementMode, SLSMainConnectionID};

struct CommandReader {
    tx: Sender<Event>,
}

impl CommandReader {
    const SOCKET_PATH: &str = "/tmp/paneru.socket";

    fn send_command(mut params: std::env::Args) -> Result<(), Error> {
        params.next();

        let output = params
            .flat_map(|param| [param.as_bytes(), &[0]].concat())
            .collect::<Vec<_>>();
        let size: u32 = output.len().try_into().unwrap();
        debug!("{}: {:?} {output:?}", function_name!(), size.to_le_bytes());

        let mut stream = UnixStream::connect(CommandReader::SOCKET_PATH)?;
        stream.write_all(&size.to_le_bytes())?;
        stream.write_all(&output)
    }

    fn new(tx: Sender<Event>) -> Self {
        CommandReader { tx }
    }

    fn start(mut self) {
        thread::spawn(move || {
            if let Err(err) = self.runner() {
                error!("{}: {err}", function_name!());
            }
        });
    }

    fn runner(&mut self) -> std::io::Result<()> {
        _ = fs::remove_file(CommandReader::SOCKET_PATH);
        let listener = UnixListener::bind(CommandReader::SOCKET_PATH)?;
        for mut stream in listener.incoming().flatten() {
            let mut buffer = [0u8; 4];
            if 4 != stream.read(&mut buffer)? {
                error!("{}: Did not read size header.", function_name!());
                break;
            }
            let size = u32::from_le_bytes(buffer) as usize;
            let mut buffer = vec![0u8; size];
            loop {
                if size != stream.read(&mut buffer)? {
                    break;
                }
                let argv = buffer
                    .split(|c| *c == 0)
                    .flat_map(|s| (!s.is_empty()).then(|| String::from_utf8_lossy(s).to_string()))
                    .collect::<Vec<_>>();
                self.tx
                    .send(Event::Command { argv })
                    .expect("command reader: error sending event");
            }
        }
        Ok(())
    }
}

fn check_separate_spaces() -> bool {
    unsafe {
        let cid = SLSMainConnectionID();
        SLSGetSpaceManagementMode(cid) == 1
    }
}

fn main() -> Result<(), Error> {
    // Set up logging (default level is INFO)
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if std::env::args().len() > 1 {
        return CommandReader::send_command(std::env::args());
    }

    if !check_separate_spaces() {
        error!(
            "{}: Option 'display has separate spaces' disabled.",
            function_name!()
        );
        return Err(Error::new(
            ErrorKind::Unsupported,
            "Option 'display has separate spaces' disabled.",
        ));
    }

    let (tx, rx) = channel::<Event>();

    CommandReader::new(tx.clone()).start();

    let mut platform_callbacks = PlatformCallbacks::new(tx.clone());
    platform_callbacks.setup_handlers()?;

    let (quit, handle) = EventHandler::new(tx.clone(), rx).start();

    platform_callbacks.run(quit);

    handle.join().expect("Can not joing threads at the end.");
    Ok(())
}
