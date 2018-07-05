extern crate tokio_core;
extern crate futures;
#[macro_use] extern crate serde_derive;
extern crate csv;
#[macro_use] extern crate failure;
extern crate irc;
extern crate serde;
extern crate inotify;

type Result<T> = ::std::result::Result<T, failure::Error>;

use std::collections::HashMap;
use irc::client::{IrcClient, ClientStream, Client, PackedIrcClient};
use inotify::EventStream;
use irc::client::ext::ClientExt;
use irc::proto::command::Command;
use irc::proto::message::Message;
use futures::{Future, Async, Poll, Stream};
use irc::client::data::Config;
use tokio_core::reactor::Core;
use inotify::{Inotify, WatchMask, EventOwned};
use csv::Reader;

#[derive(Clone)]
pub struct CombatantState {
    player: bool,
    turn: bool,
    init: u32,
    hp_cur: u32,
    hp_max: u32,
    ac: u32,
    extra: String
}
impl CombatantState {
    pub fn get_init(&self) -> String {
        if self.turn {
            format!("\x02\x0304* {:02}\x0f", self.init)
        }
        else {
            format!("- {:02}\x0f", self.init)
        }
    }
    pub fn get_ac(&self) -> String {
        if self.player {
            format!(", AC {}", self.ac)
        }
        else {
            "".into()
        }
    }
    pub fn get_extra(&self) -> String {
        if self.extra.trim() != "" {
            format!(", {}", self.extra)
        }
        else {
            "".into()
        }
    }
    pub fn get_hp(&self) -> String {
        let perc = (((self.hp_cur as f32) / (self.hp_max as f32)) * 100.0) as u32;
        let (colour, p) = match perc {
            100 => ("\x0309", "Healthy"),
            75...100 => ("\x0303", "Good"),
            50...75 => ("\x0310", "Injured"),
            25...51 => ("\x0307", "Bloodied"),
            1...25 => ("\x0305", "Mutilated"),
            0 => ("\x0304", "Dead"),
            _ => ("\x0314", "wat")
        };
        if self.player {
            format!("{}{}/{}\x0f", colour, self.hp_cur, self.hp_max)
        }
        else {
            format!("{}{}\x0f", colour, p)
        }
    }
}
pub struct State {
    combatants: HashMap<String, CombatantState>,
    order: Vec<String>,
    inot: EventStream,
    irc: PackedIrcClient,
    irc_stream: ClientStream,
    id: bool,
    ready: bool,
    file: String,
    chan: String
}
impl Future for State {
    type Item = ();
    type Error = failure::Error;

    fn poll(&mut self) -> Poll<(), failure::Error> {
        if !self.id {
            self.irc.0.identify()?;
            self.id = true;
        }
        while let Async::Ready(_) = self.irc.1.poll()? {}
        while let Async::Ready(res) = self.irc_stream.poll()? {
            let msg = res.ok_or(format_err!("irc_stream stopped"))?;
            self.handle_irc_message(msg)?;
        }
        while let Async::Ready(ine) = self.inot.poll()? {
            let ine = ine.ok_or(format_err!("inotify stopped"))?;
            self.handle_inotify(ine)?;
        }
        while let Async::Ready(_) = self.irc.1.poll()? {}
        Ok(Async::NotReady)
    }
}
impl State {
    pub fn read_file(&mut self) -> Result<Vec<CombatantRecord>> {
        let mut ret = vec![];
        println!("[+] reading {}", self.file);
        let mut rdr = Reader::from_path(&self.file)?;
        for result in rdr.deserialize() {
            let rec: CombatantRecord = result?;
            ret.push(rec);
        }
        Ok(ret)
    }
    pub fn print_combatant(&mut self, name: &str, c: CombatantState) -> Result<()> {
        if c.extra.contains("invisible") {
            return Ok(());
        }
        let desc = format!("{} <\x02{}{}\x0f> {}{}{}", c.get_init(), if !c.player { "\x0307" } else { "" }, name, c.get_hp(), c.get_extra(), c.get_ac());
        self.irc.0.send_privmsg(&self.chan, &desc)?;
        Ok(())
    }
    pub fn print_full_state(&mut self) -> Result<()> {
        self.irc.0.send_privmsg(&self.chan, &format!("\x02\x0304Initiative table:"))?;
        for name in self.order.clone() {
            let c = {
                self.combatants.get(&name).unwrap().clone()
            };
            self.print_combatant(&name, c)?;
        }
        Ok(())
    }
    pub fn update_state(&mut self) -> Result<()> {
        let mut new_order = vec![];
        let rf = match self.read_file() {
            Ok(f) => f,
            Err(e) => {
                self.irc.0.send_privmsg(&self.chan, &format!("\x02\x0304[!!] You screwed up the spreadsheet, you idiot: {:?}", e))?;
                return Ok(());
            }
        };
        for CombatantRecord { init, turn, name, player, hp_cur, hp_max, ac, extra } in rf {
            new_order.push(name.clone());
            let turn = turn.trim() != "";
            let player = player.trim() != "";
            let new = CombatantState { player, init, turn, hp_cur, hp_max, ac, extra };
            if let Some(c) = self.combatants.remove(&name) {
                let invis_old = c.extra.contains("invisible");
                let invis_new = new.extra.contains("invisible");
                if invis_old != invis_new {
                    if invis_new && !invis_old {
                        self.irc.0.send_privmsg(&self.chan, &format!("A wild \x02{}\x0f disappeared.", name))?;
                    }
                    else {
                        self.irc.0.send_privmsg(&self.chan, &format!("A wild \x02{}\x0f appeared!", name))?;
                        self.print_combatant(&name, new.clone())?;
                    }
                }
                if !invis_new {
                    if c.hp_cur != new.hp_cur {
                        let delta: i32 = c.hp_cur as i32 - hp_cur as i32;
                        self.irc.0.send_privmsg(&self.chan, &format!("\x02{}\x0F took \x02{}\x0F points of damage! ({} -> {})", name, delta, c.get_hp(), new.get_hp()))?;
                    }
                    if c.init != new.init {
                        self.irc.0.send_privmsg(&self.chan, &format!("\x02{}\x0F's initiative changed from \x02{}\x0f to \x02{}\x0f.", name, c.init, new.init))?;
                    }
                    if c.extra != new.extra {
                        if new.extra.trim() != "" {
                            self.irc.0.send_privmsg(&self.chan, &format!("\x02{}\x0f has tags: {}", name, new.extra))?;
                        }
                        else {
                        self.irc.0.send_privmsg(&self.chan, &format!("\x02{}\x0f is no longer: {}", name, c.extra))?;
                        }
                    }
                    if c.turn != new.turn {
                        if new.turn {
                            self.irc.0.send_privmsg(&self.chan, &format!("It's now \x02{}\x0F's turn.", name))?;
                            self.print_combatant(&name, new.clone())?;
                        }
                    }
                }
                self.combatants.insert(name, new);
            }
            else {
                if !new.extra.contains("invisible") {
                    self.irc.0.send_privmsg(&self.chan, &format!("Combatant \x02{}\x0f joined the encounter!", name))?;
                }
                self.print_combatant(&name, new.clone())?;
                self.combatants.insert(name, new);
            }
        }
        self.combatants.retain(|c, _| new_order.contains(c));
        self.order = new_order;
        Ok(())
    }
    pub fn on_ready(&mut self) -> Result<()> {
        for CombatantRecord { init, turn, name, player, hp_cur, hp_max, ac, extra } in self.read_file()? {
            let turn = turn.trim() != "";
            let player = player.trim() != "";
            println!("[+] reading combatant {}", name);
            self.order.push(name.clone());
            self.combatants.insert(name, CombatantState { player, init, turn, hp_cur, hp_max, ac, extra });
        }
        self.print_full_state()?;
        Ok(())
    }
    pub fn handle_inotify(&mut self, _: EventOwned) -> Result<()> {
        if !self.ready {
            return Ok(());
        }
        self.update_state()?;
        Ok(())
    }
    pub fn handle_irc_message(&mut self, m: Message) -> Result<()> {
        match m.command {
            Command::PRIVMSG(_, msg) => {
                if msg == "!table" {
                    self.print_full_state()?;
                }
                else if msg.contains("!send ") {
                    self.irc.0.send_privmsg(&self.chan, &msg.replace("!send ", ""))?;
                }
            },
            Command::JOIN(chanlist, _, _) => {
                if let Some(from) = m.prefix {
                    println!("[*] JOIN {} to {}", from, chanlist);
                    let from = from.split("!").collect::<Vec<_>>();
                    if from.len() < 1 {
                        return Ok(());
                    }
                    if from[0] == self.irc.0.current_nickname() {
                        println!("[+] Joined to channel");
                        self.ready = true;
                        self.on_ready()?;
                    }
                }
            },
            _ => {}
        }
        Ok(())
    }
}
#[derive(Debug, Deserialize)]
pub struct CombatantRecord {
    init: u32,
    turn: String,
    name: String,
    player: String,
    hp_cur: u32,
    hp_max: u32,
    ac: u32,
    extra: String
}
fn main() {
    let mut args = ::std::env::args();
    args.next().unwrap();
    let file = args.next().unwrap();
    println!("[+] file: {}", file);
    let chan = args.next().unwrap();
    println!("[+] chan: {}", chan);
    let mut core = Core::new().unwrap();
    println!("[+] loading config from config.toml");
    let cfg = Config::load("config.toml").unwrap();
    let hdl = core.handle();
    println!("[+] starting IRC client");
    let client = core.run(IrcClient::new_future(hdl, &cfg).unwrap()).unwrap();
    let stream = client.0.stream();
    println!("[*] inotifying on {}", file);
    let mut inotify = Inotify::init().unwrap();
    inotify.add_watch(&file, WatchMask::MODIFY).unwrap();
    let inot = inotify.event_stream();
    let state = State {
        combatants: HashMap::new(),
        order: vec![],
        inot,
        irc: client,
        irc_stream: stream,
        id: false,
        ready: false,
        file: file,
        chan: chan
    };
    println!("[+] running");
    core.run(state).unwrap();
}
