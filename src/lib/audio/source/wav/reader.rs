//! A thread dedicated to reading sounds from WAV files and feeding their samples to sounds on the
//! audio thread.

use audio::{self, sound};
use fxhash::FxHashMap;
use hound::{self, SampleFormat};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::BufReader;
use std::fs::File;
use std::mem;
use std::ops;
use std::path::Path;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use time_calc::Samples;

/// The number of sample buffers that the `reader` thread prepares ahead of time for a single
/// sound.
const NUM_BUFFERS: usize = 16;

/// The hound type responsible for reading samples from a WAV file.
pub type WavReader = hound::WavReader<BufReader<File>>;

/// Sends messages to the `wav::reader` thread.
pub type Tx = mpsc::Sender<Message>;

/// Receives `Message`s for the `wav::reader` thread.
pub type Rx = mpsc::Receiver<Message>;

/// For sending buffers to a sound's associated `ThreadedSamplesStream`.
pub type BufferTx = mpsc::Sender<Buffer>;

/// Receives buffers sent from the wav reader thread. Used by the `ThreadedSamplesStream` type.
pub type BufferRx = mpsc::Receiver<Buffer>;

/// A handle to the WAV reading thread.
#[derive(Clone)]
pub struct Handle {
    tx: Tx,
    thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

/// All state stored on the `wav::reader` thread.
struct Model {
    /// A map of all active WAV sounds.
    sounds: Sounds,
    /// The channel used to send messages to the reader thread.
    ///
    /// The `Model` stores this so that it may be cloned and sent with each buffer as each buffer
    /// uses this channel to send the allocated memory back to the reader thread for re-use when
    /// they have been processed.
    tx: Tx,
}

/// The type used to store sounds within the model.
type Sounds = FxHashMap<sound::Id, Sound>;

/// State related to a single wav sound.
struct Sound {
    /// A reader for reading samples from the WAV file.
    reader: WavReader,
    /// The channel used for sending buffers to the `ThreadedSampleStream` on the audio thread.
    buffer_tx: BufferTx,
    /// The list of buffers that have already been read from the file.
    ///
    /// The reader thread will ensure that the length of this `prepared_buffers` vec is always
    /// `NUM_BUFFERS`.
    prepared_buffers: VecDeque<PreparedBuffer>,
    /// Whether or not the wav reader should loop back to the beginning of the file when it reaches
    /// the end.
    looped: bool,
}

/// A buffer that has been read from a WAV file ready to be sent to the `SamplesStream` for reading
/// on the audio thread.
#[derive(Debug)]
struct PreparedBuffer {
    samples: Vec<f32>,
    // The range of WAV samples covered by `samples`.
    samples_range: ops::Range<usize>,
}

/// Messages received by the wav reader thread.
pub enum Message {
    /// When received, the reader thread will add an entry for this sound into the map and prepare
    /// the first `NUM_BUFFERS` buffers by reading samples from the given `WavReader`.
    Play(sound::Id, Play),
    /// Indicates that the sound associated with the given Id has ended.
    End(sound::Id),
    /// When received, the reader thread will re-use the given buffer to read in the next
    /// `FRAMES_PER_BUFFER` * `channels` worth of samples.
    ProcessedBuffer(sound::Id, Vec<f32>),
    /// Break from the loop as the application is closing.
    Exit,
}

/// The buffer sent to the `ThreadedSamplesStream`
///
/// When this buffer is depleted, the allocated `Vec` gets sent back to the reader thread for
/// re-use.
#[derive(Clone, Debug)]
pub struct Buffer {
    samples: Vec<f32>,
    sound_id: sound::Id,
    reader_tx: Tx,
    info: BufferInfo,
}

/// Information about this buffer within the context of a WAV file.
#[derive(Clone, Debug)]
pub struct BufferInfo {
    // The range of samples covered by this buffer.
    samples_range: ops::Range<usize>,
}

/// A message received by the reader thread for newly spawned sounds.
pub struct Play {
    /// The wav file reader.
    pub reader: WavReader,
    /// The channel used for sending buffers.
    pub buffer_tx: BufferTx,
    /// The frame from which the sound should start.
    pub start_frame: u64,
    /// Whether or not the WAV should be looped.
    pub looped: bool,
}

/// A handle to a WAV that receives the buffered samples for use on the audio thread.
pub struct SamplesStream {
    buffer_rx: BufferRx,
    buffer: RefCell<Option<Buffer>>,
    buffer_index: usize,
    wav_spec: hound::WavSpec,
    wav_len_samples: usize,
    // Whether or not the WAV is looped.
    wav_looped: bool,
}

impl Handle {
    /// Play the given sound.
    ///
    /// When called, the reader thread will add an entry for this sound into the map and prepare
    /// the first `NUM_BUFFERS` buffers by reading samples from the given `WavReader`.
    pub fn play(
        &self,
        sound_id: sound::Id,
        wav_path: &Path,
        start_frame: u64,
        looped: bool,
    ) -> Result<SamplesStream, mpsc::SendError<()>>
    {
        let reader = WavReader::open(wav_path)
            .expect("failed to read wav file");
        let wav_len_samples = reader.len() as _;
        let (buffer_tx, buffer_rx) = mpsc::channel();
        let spec = reader.spec();
        let play = Play { reader, buffer_tx, start_frame, looped };
        let samples_stream = SamplesStream::new(buffer_rx, spec, wav_len_samples, looped);
        let msg = Message::Play(sound_id, play);
        self.tx.send(msg).map_err(|_| mpsc::SendError(()))?;
        Ok(samples_stream)
    }

    /// Stop reading the wav for the sound with the given `Id`.
    pub fn end(&self, sound_id: sound::Id) -> Result<(), mpsc::SendError<()>> {
        let msg = Message::End(sound_id);
        self.tx.send(msg).map_err(|_| mpsc::SendError(()))?;
        Ok(())
    }

    /// Stops the wav reader thread and returns the raw handle to its thread.
    pub fn exit(self) -> Option<thread::JoinHandle<()>> {
        self.tx.send(Message::Exit).ok();
        self.thread.lock().unwrap().take()
    }
}

impl ops::Deref for Buffer {
    type Target = Vec<f32>;
    fn deref(&self) -> &Self::Target {
        &self.samples
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let sound_id = self.sound_id;
        // Remove the buffer from the `samples` field, replacing it with a non-allocated, empty
        // buffer.
        let read_buffer = mem::replace(&mut self.samples, Vec::new());
        let msg = Message::ProcessedBuffer(sound_id, read_buffer);
        self.reader_tx.send(msg).ok();
    }
}

impl SamplesStream {
    fn new(
        buffer_rx: BufferRx,
        wav_spec: hound::WavSpec,
        wav_len_samples: usize,
        wav_looped: bool,
    ) -> Self {
        SamplesStream {
            buffer_rx,
            buffer: RefCell::new(None),
            buffer_index: 0,
            wav_spec,
            wav_len_samples,
            wav_looped,
        }
    }

    /// The number of channels in the source audio.
    pub fn channels(&self) -> usize {
        self.wav_spec.channels as _
    }

    /// The number of frames remaining in the stream.
    pub fn remaining_frames(&self) -> Option<Samples> {
        if self.wav_looped {
            return None;
        }
        loop {
            if let Some(ref buffer) = *self.buffer.borrow() {
                let remaining_samples =
                    self.wav_len_samples - (buffer.info.samples_range.start + self.buffer_index);
                let remaining_frames = (remaining_samples / self.wav_spec.channels as usize) as _;
                return Some(Samples(remaining_frames));
            }

            let mut buffer_mut = self.buffer.borrow_mut();
            *buffer_mut = match self.buffer_rx.try_recv() {
                Err(_err) => return Some(Samples(self.wav_len_samples as _)),
                Ok(buffer) => Some(buffer),
            };
        }
    }

    /// The next sample in the stream.
    pub fn next_sample(&mut self) -> Option<f32> {
        let SamplesStream {
            ref buffer,
            ref buffer_rx,
            ref mut buffer_index,
            ..
        } = *self;

        loop {
            // If there is a sample in the current buffer, return it.
            if let Some(ref buffer) = *buffer.borrow() {
                if let Some(&sample) = buffer.get(*buffer_index) {
                    *buffer_index += 1;
                    return Some(sample);
                }
            }

            // Otherwise drop the buffer if we have one.
            //
            // This triggers the wav reader thread to re-use the buffer and enqueue it with more
            // samples.
            let mut buffer_mut = buffer.borrow_mut();
            mem::drop(buffer_mut.take());

            // Receive the next buffer.
            *buffer_mut = match buffer_rx.try_recv() {
                // If there are no more buffers, there must be no more samples so we're done.
                Err(_err) => return None,
                // Otherwise reset
                Ok(buffer) => {
                    *buffer_index = 0;
                    Some(buffer)
                },
            };
        }
    }
}

impl Iterator for SamplesStream {
    type Item = f32;
    fn next(&mut self) -> Option<Self::Item> {
        self.next_sample()
    }
}

impl Model {
    /// Initialise the `Model`.
    fn new(tx: Tx) -> Self {
        let sounds = FxHashMap::default();
        Model {
            sounds,
            tx,
        }
    }

    /// Play the given sound.
    fn play_sound(&mut self, sound_id: sound::Id, play: Play) {
        let Play { mut reader, buffer_tx, start_frame, looped } = play;

        // Seek to the given `start_frame` within the file.
        //
        // The given `frame` is the time measured as the number of samples (independent of the number
        // of channels) since the beginning of the audio data.
        //
        // If `frame` is larger than the number of samples in the file the remaining duration will be
        // wrapped around to the beginning.
        let duration_frames = reader.duration() as u64;
        let frames = start_frame % duration_frames;
        reader.seek(frames as u32)
            .expect("failed to seek to start frame in wav source");

        // Prepare the buffers for the sound.
        let wav_len_samples = reader.len() as usize;
        let prepared_buffers = (0..NUM_BUFFERS)
            .map(|_| {
                let mut samples = vec![];
                let start_sample = wav_len_samples - super::samples::remaining(&mut reader);
                fill_buffer(&mut reader, &mut samples, looped)
                    .expect("failed to fill buffer");
                let end_sample = wav_len_samples - super::samples::remaining(&mut reader);
                let samples_range = start_sample..end_sample;
                PreparedBuffer { samples, samples_range }
            })
            .collect();

        let sound = Sound {
            reader,
            buffer_tx,
            prepared_buffers,
            looped,
        };
        self.sounds.insert(sound_id, sound);

        // Send off the initial buffers.
        for _ in 0..NUM_BUFFERS {
            self.next_buffer(sound_id, vec![]).expect("failed to send initial buffer");
        }
    }

    /// Sends the next queued buffer to the `ThreadedSamplesStream` associated with the given
    /// `sound_id`.
    ///
    /// Re-uses and prepares the given processed buffer by reading samples from the WAV file
    /// associated with the given `sound::Id`, using these samples to fill the buffer and enqueue
    /// it.
    fn next_buffer(&mut self, sound_id: sound::Id, mut samples: Vec<f32>) -> Result<(), hound::Error> {
        let Model {
            ref mut sounds,
            ref tx,
        } = *self;

        // Retrieve the sound from the map.
        let sound = match sounds.get_mut(&sound_id) {
            None => return Ok(()),
            Some(sound) => sound,
        };

        let Sound {
            ref mut reader,
            ref mut prepared_buffers,
            ref buffer_tx,
            looped,
        } = *sound;

        // The total number of samples in the WAV, tracked for `BufferInfo`.
        let wav_len_samples = reader.len() as usize;

        // First, send the next queued buffer over the channel.
        if let Some(PreparedBuffer { samples, samples_range }) = prepared_buffers.pop_front() {
            let reader_tx = tx.clone();
            let info = BufferInfo { samples_range };
            let buffer = Buffer { samples, sound_id, reader_tx, info };
            // The output thread may have exited before us so ignore closed channel error.
            buffer_tx.send(buffer).ok();
        }

        // Fill the given buffer using the reader and enqueue it.
        let start = wav_len_samples - super::samples::remaining(reader);
        fill_buffer(reader, &mut samples, looped)?;
        let end = wav_len_samples - super::samples::remaining(reader);
        let samples_range = start..end;
        let prepared_buffer = PreparedBuffer { samples, samples_range };
        prepared_buffers.push_back(prepared_buffer);

        Ok(())
    }
}

/// Fill the given `samples` buffer with `FRAMES_PER_BUFFER * channels` samples read from the
/// `reader`.
fn fill_buffer(
    reader: &mut WavReader,
    samples: &mut Vec<f32>,
    looped: bool,
) -> Result<(), hound::Error> {
    let spec = reader.spec();
    let channels = spec.channels as usize;
    let num_samples = audio::FRAMES_PER_BUFFER * channels;
    samples.clear();
    if looped {
        for _ in 0..num_samples {
            let sample = read_next_sample_cycled(reader, &spec)?;
            samples.push(sample);
        }
    } else {
        for _ in 0..num_samples {
            match read_next_sample(reader, &spec)? {
                Some(sample) => samples.push(sample),
                None => break,
            }
        }
    }
    Ok(())
}

/// The same as `read_next_sample` but rather than returning `None` after the last sample in the
/// `WAV` is read it seeks the reader back to the beginning of the file and
fn read_next_sample_cycled(
    reader: &mut WavReader,
    spec: &hound::WavSpec,
) -> Result<f32, hound::Error>
{
    loop {
        match read_next_sample(reader, spec)? {
            Some(sample) => return Ok(sample),
            None => {
                reader.seek(0)?;
            },
        }
    }
}

/// Read a single sample from the reader.
///
/// Returns `None` if the WAV is depleted or reading from the WAV incurred an error.
fn read_next_sample(
    reader: &mut WavReader,
    spec: &hound::WavSpec,
) -> Result<Option<f32>, hound::Error>
{
    // A macro to simplify requesting and returning the next sample.
    macro_rules! next_sample {
        ($T:ty) => {{
            if let Some(result) = super::samples::next(&mut reader.samples::<$T>()) {
                return result.map(|s| Some(s));
            }
        }};
    }

    loop {
        match (spec.sample_format, spec.bits_per_sample) {
            (SampleFormat::Float, 32) => next_sample!(f32),
            (SampleFormat::Int, 8) => next_sample!(i8),
            (SampleFormat::Int, 16) => next_sample!(i16),
            (SampleFormat::Int, 32) => next_sample!(i32),
            _ => {
                eprintln!(
                    "Unsupported bit depth {} - currently only 8, 16 and 32 are supported",
                    spec.bits_per_sample
                );
            },
        }
        return Ok(None);
    }
}

/// Runs the wav reader thread and returns a handle to it that may be used to play or seek sounds
/// via their unique `Id`.
pub fn spawn() -> Handle {
    let (tx, rx) = mpsc::channel();
    let tx2 = tx.clone();
    let thread = thread::Builder::new()
        .name("wav_reader".into())
        .spawn(move || run(tx2, rx))
        .unwrap();
    let thread = Arc::new(Mutex::new(Some(thread)));
    Handle { tx, thread }
}

fn run(tx: Tx, rx: Rx) {
    let mut model = Model::new(tx);
    for msg in rx {
        match msg {
            Message::Play(id, play) => {
                model.play_sound(id, play);
            },
            Message::End(id) => {
                mem::drop(model.sounds.remove(&id));
            },
            Message::ProcessedBuffer(id, buffer) => {
                model.next_buffer(id, buffer)
                    .expect("failed to process next buffer");
            },
            Message::Exit => break,
        }
    }
}
