use audio;
use installation::Installation;
use metres::Metres;
use nannou::math::Point2;
use std::collections::{HashMap, HashSet};
use std::sync::{atomic, mpsc, Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time;

const TICK_RATE_MS: u64 = 16;

// The kinds of messages received by the soundscape thread.
enum Message {
    // Updates to the soundscape state from other threads.
    Update(UpdateFn),
    // Steps forward the soundscape.
    Tick(Tick),
    // Play all active sounds.
    Play,
    // Pause all active sounds.
    Pause,
    // Stop running the soundscape and exit.
    Exit,
}

#[derive(Copy, Clone, Debug)]
struct Tick {
    instant: time::Instant,
    since_last_tick: time::Duration,
    /// The total duration over which the soundscape has played.
    ///
    /// This does not increase when the stream is paused.
    playback_duration: time::Duration,
}

/// The update function applied to a source.
///
/// This is a workaround for the current inability to call a `Box<FnOnce>`
pub struct UpdateFn {
    function: Box<FnMut(&mut Model) + Send>,
}

/// The handle to the soundscape that can be used and shared amonth the main thread.
#[derive(Clone)]
pub struct Soundscape {
    tx: mpsc::Sender<Message>,
    /// Keep the thread handle in an `Option` so we can take it from the mutex upon exit.
    thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
    /// Whether or not the soundscape is currently playing.
    is_playing: Arc<AtomicBool>,
}

/// Data related to a single speaker that is relevant to the soundscape.
#[derive(Clone, Debug)]
pub struct Speaker {
    /// The position of the speaker in metres.
    pub point: Point2<Metres>,
    /// All installations assigned to the speaker.
    pub installations: HashSet<Installation>,
}

/// Properties of an audio source that are relevant to the soundscape thread.
pub struct Source {
    pub kind: audio::source::Kind,
    pub installations: HashSet<Installation>,
    pub spread: Metres,
    pub radians: f32,
}

/// Represents a currently active sound spawned by the soundscape thread.
pub struct ActiveSound {
    // movement: fn(Tick) -> (Point2<Metres>, f32)
    // TODO: We can probably remove these as we can always get them from `movement`.
    /// The current location of the sound.
    point: Point2<Metres>,
    /// The direction the sound is facing in radians.
    direction_radians: f32,
    /// The handle associated with this sound.
    handle: audio::sound::Handle,
}

/// The model containing all state running on the soundscape thread.
pub struct Model {
    /// All sources available to the soundscape for producing audio.
    sources: HashMap<audio::source::Id, Source>,
    /// All speakers within the exhibition.
    speakers: HashMap<audio::speaker::Id, Speaker>,
    /// All sounds currently being played that were spawned by the soundscape thread.
    active_sounds: HashMap<audio::sound::Id, ActiveSound>,
    /// This is used to determine the "area" for each installation.
    installation_speakers: HashMap<Installation, audio::speaker::Id>,
    /// A handle for submitting new sounds to the output stream.
    audio_output_stream: audio::output::Stream,
    /// For generating unique IDs for each new sound.
    sound_id_gen: audio::sound::IdGenerator,
    // A handle to the ticker thread.
    tick_thread: thread::JoinHandle<()>,
}

impl Speaker {
    pub fn from_audio_speaker(s: &audio::Speaker) -> Self {
        Speaker {
            point: s.point,
            installations: s.installations.clone(),
        }
    }
}

impl Source {
    /// Create a `soundscape::Source` from an `audio::Source`.
    ///
    /// Returns `None` if the given audio source does not have the `Soundscape` role.
    pub fn from_audio_source(source: &audio::Source) -> Option<Self> {
        let installations = match source.role {
            Some(audio::source::Role::Soundscape(ref soundscape)) => {
                soundscape.installations.clone()
            },
            _ => return None,
        };
        let kind = source.kind.clone();
        let spread = source.spread;
        let radians = source.radians;
        Some(Source {
            installations,
            kind,
            spread,
            radians,
        })
    }
}

impl Soundscape {
    /// Send a `FnOnce(&mut Model)` function to update the soundscape thread model.
    pub fn send<F>(&self, update: F) -> Result<(), mpsc::SendError<()>>
    where
        F: FnOnce(&mut Model) + Send + 'static,
    {
        let update = UpdateFn::from(update);
        let msg = Message::Update(update);
        if let Err(mpsc::SendError(_)) = self.tx.send(msg) {
            return Err(mpsc::SendError(()));
        }
        Ok(())
    }

    /// Whether or not the soundscape is currently playing.
    pub fn is_playing(&self) -> bool {
        self.is_playing.load(atomic::Ordering::Relaxed)
    }

    /// Pauses the soundscape playback.
    ///
    /// Returns `false` if it was already paused.
    pub fn pause(&self) -> Result<bool, mpsc::SendError<()>> {
        let result = !self.is_playing() != false;
        let msg = Message::Pause;
        self.is_playing.store(false, atomic::Ordering::Relaxed);
        self.tx.send(msg).map(|_| result).map_err(|_| mpsc::SendError(()))
    }

    /// Plays the soundscape.
    pub fn play(&self) -> Result<bool, mpsc::SendError<()>> {
        let result = self.is_playing() != true;
        let msg = Message::Play;
        self.is_playing.store(true, atomic::Ordering::Relaxed);
        self.tx.send(msg).map(|_| result).map_err(|_| mpsc::SendError(()))
    }

    /// Stops the soundscape thread and returns the raw handle to its thread.
    pub fn exit(self) -> Option<thread::JoinHandle<()>> {
        self.tx.send(Message::Exit).ok();
        self.thread.lock().unwrap().take()
    }
}

impl Model {
    /// Insert a speaker into the inner map.
    pub fn insert_speaker(&mut self, id: audio::speaker::Id, speaker: Speaker) -> Option<Speaker> {
        self.speakers.insert(id, speaker)
    }

    /// Updates the speaker with the given function.
    ///
    /// Returns `false` if the speaker wasn't there.
    pub fn update_speaker<F>(&mut self, id: &audio::speaker::Id, update: F) -> bool
    where
        F: FnOnce(&mut Speaker),
    {
        match self.speakers.get_mut(id) {
            None => false,
            Some(s) => {
                update(s);
                true
            },
        }
    }

    /// Remove a speaker from the inner hashmap.
    pub fn remove_speaker(&mut self, id: &audio::speaker::Id) -> Option<Speaker> {
        self.speakers.remove(id)
    }

    /// Insert a source into the inner hashmap.
    pub fn insert_source(&mut self, id: audio::source::Id, source: Source) -> Option<Source> {
        self.sources.insert(id, source)
    }

    /// Updates the source with the given function.
    ///
    /// Returns `false` if the source wasn't there.
    pub fn update_source<F>(&mut self, id: &audio::source::Id, update: F) -> bool
    where
        F: FnOnce(&mut Source),
    {
        match self.sources.get_mut(id) {
            None => false,
            Some(s) => {
                update(s);
                true
            },
        }
    }

    /// Remove a source from the inner hashmap.
    pub fn remove_source(&mut self, id: &audio::source::Id) -> Option<Source> {
        self.active_sounds.retain(|_, s| *id != s.handle.source_id());
        self.sources.remove(id)
    }
}

impl UpdateFn {
    // Consume self and call the update function with the given source.
    fn call(mut self, model: &mut Model) {
        (self.function)(model)
    }
}

impl<F> From<F> for UpdateFn
where
    F: FnOnce(&mut Model) + Send + 'static,
{
    fn from(f: F) -> Self {
        let mut f_opt = Some(f);
        let fn_mut = move |source: &mut Model| {
            if let Some(f) = f_opt.take() {
                f(source);
            }
        };
        UpdateFn {
            function: Box::new(fn_mut) as _,
        }
    }
}

/// Spawn the "soundscape" thread and return a handle to it.
///
/// The role of the soundscape thread is as follows:
///
/// 1. Decide when to introduce new sounds based on the properties of the currently playing sounds.
/// 2. Compose `Sound`s from a stack of `Source` -> `[Effect]`.
/// 3. Compose the path of travel through the space (including rotations for multi-channel sounds).
/// 4. Send the `Sound`s to the audio thread and accompanying monitoring stuff to the GUI thread
///    (for tracking positions, RMS, etc).
pub fn spawn(
    audio_output_stream: audio::output::Stream,
    sound_id_gen: audio::sound::IdGenerator,
) -> Soundscape {
    let (tx, rx) = mpsc::channel();
    let is_playing = Arc::new(AtomicBool::new(true));

    // Spawn a thread to generate and send ticks.
    let tick_tx = tx.clone();
    let tick_is_playing = is_playing.clone();
    let tick_thread = thread::Builder::new()
        .name("soundscape_ticker".into())
        .stack_size(512) // 512 bytes - a tiny stack for a tiny job.
        .spawn(move || {
            let mut last = time::Instant::now();
            let mut playback_duration = time::Duration::from_secs(0);
            loop {
                thread::sleep(time::Duration::from_millis(TICK_RATE_MS));
                let instant = time::Instant::now();
                let since_last_tick = instant.duration_since(last);
                if !tick_is_playing.load(atomic::Ordering::Relaxed) {
                    continue;
                }
                playback_duration += since_last_tick;
                let tick = Tick { instant, since_last_tick, playback_duration };
                if tick_tx.send(Message::Tick(tick)).is_err() {
                    break;
                }
                last = instant;
            }
        })
        .unwrap();

    // The model maintaining state between messages.
    let sources = HashMap::new();
    let speakers = HashMap::new();
    let active_sounds = HashMap::new();
    let installation_speakers = HashMap::new();
    let model = Model {
        sources,
        speakers,
        active_sounds,
        installation_speakers,
        audio_output_stream,
        sound_id_gen,
        tick_thread,
    };

    // Spawn the soundscape thread.
    let thread = thread::Builder::new()
        .name("soundscape".into())
        .spawn(move || run(model, rx))
        .unwrap();
    let thread = Arc::new(Mutex::new(Some(thread)));
    Soundscape { tx, thread, is_playing }
}

// A blocking function that is run on the unique soundscape thread (called by spawn).
fn run(mut model: Model, msg_rx: mpsc::Receiver<Message>) {
    // Wait for messages.
    for msg in msg_rx {
        match msg {
            // An update from another thread.
            Message::Update(update) => update.call(&mut model),

            // Break from the loop and finish the thread.
            Message::Exit => break,

            // Step forward the state of the soundscape.
            Message::Tick(t) => tick(&mut model, t),

            // Play all active sounds.
            Message::Play => {
                for sound in model.active_sounds.values() {
                    sound.handle.play();
                }
            },

            // Pause all active sounds.
            Message::Pause => {
                for sound in model.active_sounds.values() {
                    sound.handle.pause();
                }
            }
        }
    }
}

// Called each time the soundscape thread receives a tick.
fn tick(_model: &mut Model, _tick: Tick) {
}
