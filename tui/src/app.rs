use std::net::SocketAddr;

use anyhow::Ok;
use base64::{prelude::BASE64_STANDARD, Engine};
use common::{RoomEvent, RoomName, ServerCommand, ServerEvent, Username};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::{SinkExt, StreamExt};
use ratatui::{style::Style, DefaultTerminal};
use ratatui_explorer::File;
use tokio::{
    net::{tcp::OwnedWriteHalf, TcpStream},
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use tui_textarea::{Input, Key, TextArea};

use crate::popup::Popup;
use crate::room_list::RoomList;
use crate::{logger::Logger, message_list::MessageList};

pub struct App {
    addr: SocketAddr,
    term_stream: EventStream,
    is_running: bool,
    event_sender: UnboundedSender<Event>,
    event_receiver: UnboundedReceiver<Event>,
    tcp_writer: Option<FramedWrite<OwnedWriteHalf, LinesCodec>>,
    // UI components (these need to be public as we define the draw_ui method not in a child module)
    pub message_list: MessageList,
    pub room_list: RoomList,
    pub text_area: TextArea<'static>,
    pub logger: Option<Logger>,
    pub popup: Option<Popup>,
}

#[derive(Clone)]
pub enum Event {
    Terminal(CrosstermEvent),
    FileSelected(File),
    PopupClosed,
    LoggerClosed,
    EffectRendered,
}

impl From<CrosstermEvent> for Event {
    fn from(event: CrosstermEvent) -> Self {
        Event::Terminal(event)
    }
}

impl App {
    pub fn new(addr: SocketAddr) -> Self {
        let (event_sender, event_receiver) = unbounded_channel();
        let term_stream = EventStream::new();
        Self {
            addr,
            term_stream,
            is_running: false,
            event_sender,
            event_receiver,
            tcp_writer: None,
            message_list: MessageList::default(),
            room_list: RoomList::default(),
            text_area: create_text_area(),
            logger: None,
            popup: None,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> anyhow::Result<()> {
        self.is_running = true;

        let connection = TcpStream::connect(self.addr).await?;
        let (reader, writer) = connection.into_split();
        self.tcp_writer = Some(FramedWrite::new(writer, LinesCodec::new()));
        let mut tcp_reader = FramedRead::new(reader, LinesCodec::new());

        while self.is_running {
            terminal.draw(|frame| self.draw_ui(frame))?;
            tokio::select! {
                Some(crossterm_event) = self.term_stream.next() => {
                    let crossterm_event = crossterm_event?;
                    self.handle_event(Event::from(crossterm_event)).await?;
                },
                Some(event) = self.event_receiver.recv() => self.handle_event(event).await?,
                Some(tcp_event) = tcp_reader.next() => self.handle_server_event(tcp_event?).await?,
            }
        }
        Ok(())
    }

    pub async fn send(&mut self, command: ServerCommand) {
        let framed = self.tcp_writer.as_mut().unwrap();
        let _ = framed.send(command.to_string()).await;
    }

    pub async fn handle_event(&mut self, event: Event) -> anyhow::Result<()> {
        match event {
            Event::Terminal(raw_event) => {
                let input = Input::from(raw_event.clone());
                if let Some(ref mut popup) = self.popup {
                    popup.handle_input(input, raw_event).await?;
                    return Ok(());
                }
                if let Some(logger) = &mut self.logger {
                    logger.handle_input(input).await?;
                    return Ok(());
                }
                self.handle_key_input(input).await?;
            }
            // Send file to server
            Event::FileSelected(file) => {
                self.popup = None;
                let contents = tokio::fs::read(file.path()).await?;
                let base64 = BASE64_STANDARD.encode(contents);
                let command = ServerCommand::SendFile(file.name().to_string(), base64);
                self.send(command).await;
            }
            Event::PopupClosed => {
                self.popup = None;
            }
            Event::LoggerClosed => {
                self.logger = None;
            }
            Event::EffectRendered => {}
        }

        Ok(())
    }

    async fn handle_key_input(&mut self, input: Input) -> anyhow::Result<(), anyhow::Error> {
        match (input.ctrl, input.key) {
            (_, Key::Esc) => self.is_running = false,
            (_, Key::Enter) => self.send_message().await?,
            (_, Key::Down) => self.message_list.state.select_previous(),
            (_, Key::Up) => self.message_list.state.select_next(),
            (true, Key::Char('e')) => self.show_file_explorer()?,
            (true, Key::Char('p')) => self.preview_file()?,
            (true, Key::Char('l')) => self.show_logger(),
            (_, _) => {
                let _ = self.text_area.input_without_shortcuts(input);
            }
        }
        Ok(())
    }

    async fn send_message(&mut self) -> Result<(), anyhow::Error> {
        let sink = self.tcp_writer.as_mut().unwrap();
        if !self.text_area.is_empty() {
            for line in self.text_area.clone().into_lines() {
                sink.send(line).await?;
            }
            self.text_area.select_all();
            self.text_area.delete_line_by_end();
        }
        Ok(())
    }

    fn show_file_explorer(&mut self) -> Result<(), anyhow::Error> {
        let popup = Popup::file_explorer(self.event_sender.clone())?;
        self.popup = Some(popup);
        Ok(())
    }

    fn show_logger(&mut self) {
        self.logger = Some(Logger::new(self.event_sender.clone()));
    }

    fn preview_file(&mut self) -> Result<(), anyhow::Error> {
        let selected_event = self.message_list.selected_event();
        let event_sender = self.event_sender.clone();
        if let Some(ServerEvent::RoomEvent {
            event: RoomEvent::File { filename, contents },
            ..
        }) = selected_event
        {
            let popup = if filename.ends_with("png") {
                Popup::image_preview(contents, event_sender)
            } else {
                Popup::markdown_preview(contents, event_sender)
            }?;
            self.popup = Some(popup);
        }
        Ok(())
    }

    pub async fn handle_server_event(&mut self, event: String) -> anyhow::Result<()> {
        let event = ServerEvent::from_json_str(&event)?;
        tracing::info!("Handling server event: {event:?}");
        self.message_list.events.push(event.clone());
        match event {
            ServerEvent::CommandHelp(username, _help) => self.message_list.username = username,
            ServerEvent::RoomEvent {
                room_name,
                username,
                event,
            } => self.handle_room_event(room_name, username, event).await,
            ServerEvent::Error(_error) => {}
            ServerEvent::Rooms(rooms) => {
                let names = rooms.iter().cloned().map(|(name, _count)| name).collect();
                self.room_list.rooms = names
            }
            ServerEvent::RoomCreated(room_name) => {
                self.room_list.push_room(room_name);
            }
            ServerEvent::RoomDeleted(room_name) => {
                self.room_list.remove_room(&room_name);
            }
            ServerEvent::Users(users) => self.room_list.users = users,
            ServerEvent::Disconnect => {
                self.is_running = false;
            }
        }
        Ok(())
    }

    async fn handle_room_event(
        &mut self,
        _room_name: RoomName,
        username: Username,
        room_event: RoomEvent,
    ) {
        match room_event {
            RoomEvent::Message(_message) => {}
            RoomEvent::Joined(room) | RoomEvent::Left(room) => {
                self.message_list.room_name = room.clone();
                self.room_list.room_name = room;
                self.send(ServerCommand::ListUsers).await;
                self.send(ServerCommand::ListRooms).await;
            }
            RoomEvent::NameChange(new_username) => {
                if username == self.message_list.username {
                    self.message_list.username = new_username;
                } else {
                    self.send(ServerCommand::ListUsers).await;
                }
            }
            RoomEvent::Nudge(username) => {
                if username == self.message_list.username {
                    self.popup = Some(Popup::effect(self.event_sender.clone()));
                }
            }
            RoomEvent::File { .. } => {}
        }
    }
}

fn create_text_area() -> TextArea<'static> {
    let mut text_area = TextArea::default();
    text_area.set_cursor_line_style(Style::default());
    text_area.set_placeholder_text("Start typing...");
    text_area
}
