#![forbid(unsafe_code)]

mod config;

use anyhow::Result;
use config::Config;
use lazy_static::lazy_static;
use log::{debug, error, info, trace, warn};
use regex::Regex;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{stream::StreamExt as _, time::delay_for};
use twitchchat::{events, messages, Channel, Control, Dispatcher, IntoChannel, Runner};

lazy_static! {
    static ref CONFIG: Config = Config::from_path("config.ron").unwrap();
    static ref ENTER_SUCCESS_PATTERN: Regex = Regex::new(r"\| (No experience gained! FeelsBadMan|Experience Gained: (\d+))").unwrap();
    static ref ENTER_FAIL_PATTERN: Regex = Regex::new(
        r"you have already entered the dungeon recently, (\d):(\d\d):(\d\d) left until you can enter again!"
    ).unwrap();
    static ref RAID_PATTERN: Regex = Regex::new(r"A Raid Event at Level \[(\d+)\] has appeared\.").unwrap();
}

#[derive(Clone)]
struct Bot {
    control: Control,
    wait_for_reply: Arc<AtomicBool>,
    channel: Channel,
    retries: Arc<AtomicU8>,
    sent_byte: bool,
}

impl Bot {
    async fn run(mut self, dispatcher: Dispatcher) {
        // subscribe to the events we're interested in
        let mut events = dispatcher.subscribe::<events::Privmsg>();

        // and wait for a specific event (blocks the current task)
        let ready = dispatcher.wait_for::<events::IrcReady>().await.unwrap();
        info!("Connected! My username is {}", ready.nickname);

        let writer = self.control.writer();

        // and then join a channel
        info!("Joining target channel (name: {})", self.channel.as_str());
        writer.join(self.channel.clone()).await.unwrap();

        // join everytime we reconnect
        let mut reconnect = dispatcher.subscribe::<events::IrcReady>();
        let mut writer = writer.clone();
        let channel = self.channel.clone();
        tokio::spawn(async move {
            while let Some(msg) = reconnect.next().await {
                info!("Reconnected as {}", msg.nickname);
                writer.join(channel.clone()).await.unwrap()
            }
        });

        // Send initial message
        self.enter_dungeon();

        // and then our 'main loop'
        while let Some(msg) = events.next().await {
            trace!("Handling Message");
            if !self.handle(&*msg).await {
                return;
            }
            trace!("Finished handling Message");
        }
    }

    fn enter_dungeon_delayed(&mut self, duration: Duration) {
        info!("Entering dungeon in {} seconds", duration.as_secs());

        let mut bot = self.clone();
        tokio::spawn(async move {
            delay_for(duration).await;
            bot.enter_dungeon();
        });
    }

    fn enter_dungeon(&mut self) {
        let control = self.control.clone();
        let wait_for_reply = self.wait_for_reply.clone();
        let retries = self.retries.clone();
        let mut bot = self.clone();

        tokio::spawn(async move {
            retries.store(0, Ordering::Relaxed);

            loop {
                trace!(
                    "Trying to enter dungeon (retries: {})",
                    retries.load(Ordering::Relaxed)
                );

                bot.send_message("+ed").await;

                wait_for_reply.store(true, Ordering::Relaxed);

                delay_for(Duration::from_secs(
                    (retries.load(Ordering::Relaxed) * 4 + 3) as u64,
                ))
                .await;

                if wait_for_reply.load(Ordering::Relaxed) {
                    if retries.fetch_add(1, Ordering::Relaxed) >= 3 {
                        error!("Could not communicate with HuwoBot");
                        control.stop();

                        return;
                    }

                    warn!("Did not get an answer from HuwoBot. Retrying");

                    // continue normally
                    continue; // explicit
                } else {
                    return;
                }
            }
        });
    }

    async fn handle(&mut self, msg: &messages::Privmsg<'_>) -> bool {
        if msg.user_id() != Some(64313471) {
            debug!("Sender is not HuwoBot");

            return true;
        }

        if self.wait_for_reply.load(Ordering::Relaxed) {
            if !msg
                .data
                .to_lowercase()
                .starts_with(&CONFIG.clone().username)
            {
                debug!("Message is not meant for us");

                return true;
            }

            if let Some(captures) = ENTER_SUCCESS_PATTERN.captures(&*msg.data) {
                match captures.get(2) {
                    Some(xp) => info!("Gained {} experience", xp.as_str()),
                    None => info!("No Experience gained"),
                }

                self.wait_for_reply.store(false, Ordering::Relaxed);
                self.retries.store(0, Ordering::Relaxed);

                // Wait for 1h and 3s. The cooldown for +enterdungeon is 1 hour.
                // HuwoBot doesn't like beeing on time, but this bot is german,
                // so we wait a additional 3 seconds.
                self.enter_dungeon_delayed(Duration::from_secs(3603));

                return true;
            }
            if let Some(captures) = ENTER_FAIL_PATTERN.captures(&*msg.data) {
                let hours: u64 = captures.get(1).unwrap().as_str().parse().unwrap();
                let minutes: u64 = captures.get(2).unwrap().as_str().parse().unwrap();
                let seconds: u64 = captures.get(3).unwrap().as_str().parse().unwrap();

                info!(
                    "Time until next adventure: {}:{:02}:{:02}",
                    hours, minutes, seconds
                );

                self.wait_for_reply.store(false, Ordering::Relaxed);
                self.retries.store(0, Ordering::Relaxed);

                self.enter_dungeon_delayed(Duration::from_secs(
                    hours * 3600 + minutes * 60 + seconds + 3, // add a additional buffer of 3 seconds
                ));

                return true;
            }

            warn!("Waiting for reply, but message did not match")
        }

        if let Some(captures) = RAID_PATTERN.captures(&*msg.data) {
            info!(
                "A raid at level {} is starting!",
                captures.get(1).unwrap().as_str()
            );

            self.send_message("+join").await;

            return true;
        }

        // todo raid result

        true // to keep the 'Bot' running
    }

    async fn send_message(&mut self, data: &str) {
        let mut data = data.to_string();
        if !self.sent_byte {
            data.push_str("\u{e0000}");
        }
        self.sent_byte = !self.sent_byte;

        self.control
            .writer()
            .privmsg(self.channel.clone(), &data)
            .await
            .unwrap();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let dispatcher = Dispatcher::new();
    let (mut runner, control) = Runner::new(dispatcher.clone());

    // make a bot and get a future to its main loop
    let bot = Bot {
        control,
        wait_for_reply: Arc::new(AtomicBool::new(false)),
        channel: CONFIG.channel.clone().into_channel().unwrap(),
        retries: Arc::new(AtomicU8::new(0)),
        sent_byte: false,
    }
    .run(dispatcher);

    // create a connector, this can be used to reconnect.
    let connector = twitchchat::Connector::new(|| async move {
        twitchchat::native_tls::connect_easy(
            CONFIG.username.clone().as_str(),
            CONFIG.token.clone().as_str(),
        )
        .await
    });

    // and run the dispatcher/writer loop
    //
    // this uses a retry strategy that'll reconnect with the connect.
    // using the `run_with_retry` method will consume the 'Status' types
    let done = runner.run_with_retry(connector, twitchchat::RetryStrategy::on_timeout);

    // and select over our two futures
    tokio::select! {
        // wait for the bot to complete
        _ = bot => { info!("done running the bot") }
        // or wait for the runner to complete
        status = done => {
            match status {
                Ok(()) => { info!("we're done") }
                Err(err) => { error!("error running: {}", err) }
            }
        }
    }

    Ok(())
}
