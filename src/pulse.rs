use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{mpsc, Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use libpulse_binding as pa;
use pa::callbacks::ListResult;
use pa::context::subscribe::{Facility, InterestMaskSet, Operation as SubscribeOperation};
use pa::context::{Context as PulseContext, FlagSet as ContextFlagSet, State as ContextState};
use pa::mainloop::standard::{IterateResult, Mainloop};
use pa::operation::{Operation, State as OperationState};
use pa::proplist::{properties::APPLICATION_NAME, Proplist};
use pa::volume::{ChannelVolumes, Volume, VolumeDB};
use tokio::sync::{oneshot, watch};
use tracing::{debug, info};

use crate::audio_backend::{AudioBackend, BackendFuture, SinkDescriptor, SinkState, StreamState};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const OPERATION_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_INTERVAL: Duration = Duration::from_millis(50);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

const STREAM_PROPERTIES: &[&str] = &[
    "application.name",
    "application.process.binary",
    "application.process.id",
    "media.role",
    "media.title",
    "media.name",
];

enum PulseEvent {
    Sink(Option<SubscribeOperation>, u32),
    Topology,
}

type PulseEventQueue = Rc<RefCell<VecDeque<PulseEvent>>>;

enum Request {
    GetSink {
        name: String,
        reply: oneshot::Sender<Result<SinkState>>,
    },
    ListSinks {
        reply: oneshot::Sender<Result<Vec<SinkDescriptor>>>,
    },
    ListSinkInputs {
        reply: oneshot::Sender<Result<Vec<StreamState>>>,
    },
    SetSinkPercent {
        name: String,
        values: Vec<f64>,
        reply: oneshot::Sender<Result<SinkState>>,
    },
    SetSinkDb {
        name: String,
        db: f64,
        reply: oneshot::Sender<Result<SinkState>>,
    },
    SetSinkMute {
        name: String,
        muted: bool,
        reply: oneshot::Sender<Result<SinkState>>,
    },
    SetSinkInputPercent {
        index: u32,
        percent: f64,
        reply: oneshot::Sender<Result<StreamState>>,
    },
    SetSinkInputMute {
        index: u32,
        muted: bool,
        reply: oneshot::Sender<Result<StreamState>>,
    },
}

#[derive(Clone)]
pub struct PulseControl {
    sender: mpsc::Sender<Request>,
    cache: Arc<RwLock<HashMap<String, SinkState>>>,
    changes: watch::Sender<u64>,
}

impl PulseControl {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cache = Arc::new(RwLock::new(HashMap::new()));
        let worker_cache = cache.clone();
        let (changes, _) = watch::channel(0_u64);
        let worker_changes = changes.clone();

        thread::Builder::new()
            .name("proaudio-pulse-control".into())
            .spawn(move || worker_loop(receiver, worker_cache, worker_changes))
            .context("failed to spawn persistent PulseAudio control thread")?;

        Ok(Self {
            sender,
            cache,
            changes,
        })
    }

    async fn request<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<Result<T>>) -> Request,
    ) -> Result<T> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(request(reply))
            .map_err(|_| anyhow!("PulseAudio control worker stopped"))?;
        receiver
            .await
            .map_err(|_| anyhow!("PulseAudio control worker dropped reply"))?
    }

    pub async fn sink_state(&self, name: &str) -> Result<SinkState> {
        if let Ok(cache) = self.cache.read() {
            if let Some(state) = cache.get(name).cloned() {
                return Ok(state);
            }
        }
        let name = name.to_owned();
        self.request(|reply| Request::GetSink { name, reply }).await
    }

    pub async fn list_sinks(&self) -> Result<Vec<SinkDescriptor>> {
        self.request(|reply| Request::ListSinks { reply }).await
    }

    pub async fn list_sink_inputs(&self) -> Result<Vec<StreamState>> {
        self.request(|reply| Request::ListSinkInputs { reply })
            .await
    }

    pub async fn set_percent_channels(&self, name: &str, values: &[f64]) -> Result<SinkState> {
        let name = name.to_owned();
        let values = values.to_vec();
        self.request(|reply| Request::SetSinkPercent {
            name,
            values,
            reply,
        })
        .await
    }

    pub async fn set_db(&self, name: &str, db: f64) -> Result<SinkState> {
        let name = name.to_owned();
        self.request(|reply| Request::SetSinkDb { name, db, reply })
            .await
    }

    pub async fn set_mute(&self, name: &str, muted: bool) -> Result<SinkState> {
        let name = name.to_owned();
        self.request(|reply| Request::SetSinkMute { name, muted, reply })
            .await
    }

    pub async fn set_sink_input_percent(&self, index: u32, percent: f64) -> Result<StreamState> {
        self.request(|reply| Request::SetSinkInputPercent {
            index,
            percent,
            reply,
        })
        .await
    }

    pub async fn set_sink_input_mute(&self, index: u32, muted: bool) -> Result<StreamState> {
        self.request(|reply| Request::SetSinkInputMute {
            index,
            muted,
            reply,
        })
        .await
    }
}

impl AudioBackend for PulseControl {
    fn backend_name(&self) -> &'static str {
        "pulse"
    }

    fn sink_state<'a>(&'a self, name: &'a str) -> BackendFuture<'a, SinkState> {
        Box::pin(PulseControl::sink_state(self, name))
    }

    fn list_sinks(&self) -> BackendFuture<'_, Vec<SinkDescriptor>> {
        Box::pin(PulseControl::list_sinks(self))
    }

    fn list_sink_inputs(&self) -> BackendFuture<'_, Vec<StreamState>> {
        Box::pin(PulseControl::list_sink_inputs(self))
    }

    fn set_sink_percent_channels<'a>(
        &'a self,
        name: &'a str,
        values: &'a [f64],
    ) -> BackendFuture<'a, SinkState> {
        Box::pin(PulseControl::set_percent_channels(self, name, values))
    }

    fn set_sink_db<'a>(&'a self, name: &'a str, db: f64) -> BackendFuture<'a, SinkState> {
        Box::pin(PulseControl::set_db(self, name, db))
    }

    fn set_sink_mute<'a>(&'a self, name: &'a str, muted: bool) -> BackendFuture<'a, SinkState> {
        Box::pin(PulseControl::set_mute(self, name, muted))
    }

    fn set_sink_input_percent(&self, index: u32, percent: f64) -> BackendFuture<'_, StreamState> {
        Box::pin(PulseControl::set_sink_input_percent(self, index, percent))
    }

    fn set_sink_input_mute(&self, index: u32, muted: bool) -> BackendFuture<'_, StreamState> {
        Box::pin(PulseControl::set_sink_input_mute(self, index, muted))
    }

    fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

fn volume_from_percent(percent: f64) -> Volume {
    let percent = percent.clamp(0.0, 100.0);
    Volume(((Volume::NORMAL.0 as f64) * percent / 100.0).round() as u32)
}

fn percent_from_volume(volume: Volume) -> f64 {
    volume.0 as f64 * 100.0 / Volume::NORMAL.0 as f64
}

fn db_from_volume(volume: Volume) -> f64 {
    let db = VolumeDB::from(volume).0;
    if db.is_finite() {
        db
    } else {
        -200.0
    }
}

fn state_from_info(info: &pa::context::introspect::SinkInfo<'_>) -> Result<SinkState> {
    let name = info
        .name
        .as_deref()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("PulseAudio sink without a name"))?;

    Ok(SinkState {
        name,
        index: info.index,
        volumes_percent: info
            .volume
            .get()
            .iter()
            .copied()
            .map(percent_from_volume)
            .collect(),
        volumes_db: info
            .volume
            .get()
            .iter()
            .copied()
            .map(db_from_volume)
            .collect(),
        muted: info.mute,
    })
}

fn descriptor_from_info(info: &pa::context::introspect::SinkInfo<'_>) -> Result<SinkDescriptor> {
    let state = state_from_info(info)?;
    let description = info
        .description
        .as_deref()
        .unwrap_or(state.name.as_str())
        .to_owned();
    let device_class = info.proplist.get_str("device.class").unwrap_or_default();
    let alsa_card = info
        .proplist
        .get_str("alsa.card")
        .and_then(|value| value.parse::<u32>().ok());
    let alsa_device = info
        .proplist
        .get_str("alsa.device")
        .and_then(|value| value.parse::<u32>().ok());
    let channel_map = info
        .channel_map
        .get()
        .iter()
        .map(|position| position.to_string().into_owned())
        .collect();

    Ok(SinkDescriptor {
        state,
        description,
        device_class,
        alsa_card,
        alsa_device,
        sample_format: info.sample_spec.format.to_string().into_owned(),
        sample_rate: info.sample_spec.rate,
        channels: info.sample_spec.channels,
        channel_map,
        device_api: info.proplist.get_str("device.api").unwrap_or_default(),
        device_bus: info.proplist.get_str("device.bus").unwrap_or_default(),
        state_name: format!("{:?}", info.state).to_ascii_lowercase(),
    })
}

fn stream_from_info(info: &pa::context::introspect::SinkInputInfo<'_>) -> StreamState {
    let properties = STREAM_PROPERTIES
        .iter()
        .filter_map(|key| {
            info.proplist
                .get_str(key)
                .filter(|value| !value.is_empty())
                .map(|value| ((*key).to_owned(), value))
        })
        .collect();

    StreamState {
        index: info.index,
        sink: info.sink,
        name: info.name.as_deref().unwrap_or("").to_owned(),
        properties,
        volumes_percent: if info.has_volume {
            info.volume
                .get()
                .iter()
                .copied()
                .map(percent_from_volume)
                .collect()
        } else {
            Vec::new()
        },
        muted: info.mute,
        corked: info.corked,
        has_volume: info.has_volume,
        volume_writable: info.volume_writable,
    }
}

fn iterate_once(mainloop: &mut Mainloop, context: &PulseContext) -> Result<()> {
    match mainloop.iterate(false) {
        IterateResult::Success(_) => {}
        IterateResult::Quit(code) => bail!("PulseAudio mainloop quit: {code:?}"),
        IterateResult::Err(err) => bail!("PulseAudio mainloop error: {err}"),
    }

    match context.get_state() {
        ContextState::Failed | ContextState::Terminated => {
            bail!("PulseAudio context disconnected")
        }
        _ => Ok(()),
    }
}

fn wait_for_operation<C: ?Sized>(
    mainloop: &mut Mainloop,
    context: &PulseContext,
    operation: &Operation<C>,
) -> Result<()> {
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    loop {
        match operation.get_state() {
            OperationState::Done => return Ok(()),
            OperationState::Cancelled => bail!("PulseAudio operation cancelled"),
            OperationState::Running => {}
        }
        if Instant::now() >= deadline {
            bail!("PulseAudio operation timed out");
        }
        iterate_once(mainloop, context)?;
        thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_context(mainloop: &mut Mainloop, context: &PulseContext) -> Result<()> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match context.get_state() {
            ContextState::Ready => return Ok(()),
            ContextState::Failed | ContextState::Terminated => {
                bail!("PulseAudio context connection failed")
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!("PulseAudio connection timed out");
        }
        match mainloop.iterate(false) {
            IterateResult::Success(_) => {}
            IterateResult::Quit(code) => bail!("PulseAudio mainloop quit: {code:?}"),
            IterateResult::Err(err) => bail!("PulseAudio mainloop error: {err}"),
        }
        thread::sleep(Duration::from_millis(2));
    }
}

struct PulseConnection {
    mainloop: Mainloop,
    context: PulseContext,
    events: PulseEventQueue,
    cache: Arc<RwLock<HashMap<String, SinkState>>>,
    changes: watch::Sender<u64>,
}

impl PulseConnection {
    fn connect(
        cache: Arc<RwLock<HashMap<String, SinkState>>>,
        changes: watch::Sender<u64>,
    ) -> Result<Self> {
        let mut mainloop = Mainloop::new().context("failed to create PulseAudio mainloop")?;
        let mut proplist = Proplist::new().context("failed to create PulseAudio proplist")?;
        proplist
            .set_str(APPLICATION_NAME, "ProAudio Player")
            .map_err(|()| anyhow!("failed to set PulseAudio application name"))?;
        let mut context =
            PulseContext::new_with_proplist(&mainloop, "ProAudioPlayerControl", &proplist)
                .context("failed to create PulseAudio context")?;

        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("failed to connect to PulseAudio server")?;
        wait_for_context(&mut mainloop, &context)?;

        let events = Rc::new(RefCell::new(VecDeque::new()));
        let event_queue = events.clone();
        context.set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
            match facility {
                Some(Facility::Sink) => event_queue
                    .borrow_mut()
                    .push_back(PulseEvent::Sink(operation, index)),
                Some(Facility::SinkInput) | Some(Facility::Server) => {
                    event_queue.borrow_mut().push_back(PulseEvent::Topology)
                }
                _ => {}
            }
        })));

        let subscribed = Rc::new(RefCell::new(None));
        let subscribed_result = subscribed.clone();
        let mask = InterestMaskSet::SINK | InterestMaskSet::SINK_INPUT | InterestMaskSet::SERVER;
        let operation = context.subscribe(mask, move |success| {
            *subscribed_result.borrow_mut() = Some(success);
        });
        wait_for_operation(&mut mainloop, &context, &operation)?;
        if subscribed.borrow().as_ref().copied() != Some(true) {
            bail!("PulseAudio subscription failed");
        }

        info!("Persistent PulseAudio control connection established");
        Ok(Self {
            mainloop,
            context,
            events,
            cache,
            changes,
        })
    }

    fn healthy(&self) -> bool {
        self.context.get_state() == ContextState::Ready
    }

    fn notify_changes(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn cached_state(&self, name: &str) -> Option<SinkState> {
        self.cache.read().ok()?.get(name).cloned()
    }

    fn update_cache(&self, state: &SinkState) {
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(state.name.clone(), state.clone());
        }
    }

    fn remove_cached_index(&self, index: u32) {
        if let Ok(mut cache) = self.cache.write() {
            cache.retain(|_, state| state.index != index);
        }
    }

    fn current_or_query(&mut self, name: &str) -> Result<SinkState> {
        match self.cached_state(name) {
            Some(state) => Ok(state),
            None => self.query_sink_by_name(name),
        }
    }

    fn query_sink_by_name(&mut self, name: &str) -> Result<SinkState> {
        let result = Rc::new(RefCell::new(None));
        let callback_result = result.clone();
        let operation =
            self.context
                .introspect()
                .get_sink_info_by_name(name, move |item| match item {
                    ListResult::Item(info) => {
                        *callback_result.borrow_mut() = Some(state_from_info(info));
                    }
                    ListResult::Error => {
                        *callback_result.borrow_mut() =
                            Some(Err(anyhow!("PulseAudio sink query failed")));
                    }
                    ListResult::End => {}
                });

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        let state = result
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow!("PulseAudio sink not found: {name}"))??;
        self.update_cache(&state);
        Ok(state)
    }

    fn query_sink_by_index(&mut self, index: u32) -> Result<SinkState> {
        let result = Rc::new(RefCell::new(None));
        let callback_result = result.clone();
        let operation = self
            .context
            .introspect()
            .get_sink_info_by_index(index, move |item| match item {
                ListResult::Item(info) => {
                    *callback_result.borrow_mut() = Some(state_from_info(info));
                }
                ListResult::Error => {
                    *callback_result.borrow_mut() =
                        Some(Err(anyhow!("PulseAudio sink query failed")));
                }
                ListResult::End => {}
            });

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        let state = result
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow!("PulseAudio sink index not found: {index}"))??;
        self.update_cache(&state);
        Ok(state)
    }

    fn list_sinks(&mut self) -> Result<Vec<SinkDescriptor>> {
        let items = Rc::new(RefCell::new(Vec::new()));
        let callback_items = items.clone();
        let failed = Rc::new(RefCell::new(false));
        let callback_failed = failed.clone();

        let operation = self
            .context
            .introspect()
            .get_sink_info_list(move |item| match item {
                ListResult::Item(info) => match descriptor_from_info(info) {
                    Ok(descriptor) => callback_items.borrow_mut().push(descriptor),
                    Err(err) => debug!(error = %err, "Ignoring unnamed PulseAudio sink"),
                },
                ListResult::Error => *callback_failed.borrow_mut() = true,
                ListResult::End => {}
            });

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if *failed.borrow() {
            bail!("PulseAudio sink list query failed");
        }

        let result = items.borrow().clone();
        for descriptor in &result {
            self.update_cache(&descriptor.state);
        }
        Ok(result)
    }

    fn list_sink_inputs(&mut self) -> Result<Vec<StreamState>> {
        let items = Rc::new(RefCell::new(Vec::new()));
        let callback_items = items.clone();
        let failed = Rc::new(RefCell::new(false));
        let callback_failed = failed.clone();

        let operation =
            self.context
                .introspect()
                .get_sink_input_info_list(move |item| match item {
                    ListResult::Item(info) => {
                        callback_items.borrow_mut().push(stream_from_info(info));
                    }
                    ListResult::Error => *callback_failed.borrow_mut() = true,
                    ListResult::End => {}
                });

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if *failed.borrow() {
            bail!("PulseAudio sink-input list query failed");
        }
        let result = items.borrow().clone();
        Ok(result)
    }

    fn query_sink_input(&mut self, index: u32) -> Result<StreamState> {
        let result = Rc::new(RefCell::new(None));
        let callback_result = result.clone();

        let operation =
            self.context
                .introspect()
                .get_sink_input_info(index, move |item| match item {
                    ListResult::Item(info) => {
                        *callback_result.borrow_mut() = Some(stream_from_info(info));
                    }
                    ListResult::Error | ListResult::End => {}
                });

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        let state = result
            .borrow_mut()
            .take()
            .ok_or_else(|| anyhow!("PulseAudio sink input not found: {index}"))?;
        Ok(state)
    }

    fn set_sink_volume(
        &mut self,
        name: &str,
        volume: ChannelVolumes,
        mut state: SinkState,
    ) -> Result<SinkState> {
        let success = Rc::new(RefCell::new(None));
        let callback_success = success.clone();
        let mut introspector = self.context.introspect();
        let operation = introspector.set_sink_volume_by_name(
            name,
            &volume,
            Some(Box::new(move |ok| {
                *callback_success.borrow_mut() = Some(ok);
            })),
        );

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if success.borrow().as_ref().copied() != Some(true) {
            bail!("PulseAudio rejected sink volume update for {name}");
        }

        state.volumes_percent = volume
            .get()
            .iter()
            .copied()
            .map(percent_from_volume)
            .collect();
        state.volumes_db = volume.get().iter().copied().map(db_from_volume).collect();
        self.update_cache(&state);
        Ok(state)
    }

    fn set_sink_percent(&mut self, name: &str, values: &[f64]) -> Result<SinkState> {
        if values.is_empty() {
            bail!("empty PulseAudio volume update");
        }

        let current = self.current_or_query(name)?;
        let channels = current.volumes_percent.len().max(1);
        let channels =
            u8::try_from(channels).map_err(|_| anyhow!("too many PulseAudio channels"))?;
        let fallback = values[0];
        let mut volume = ChannelVolumes::default();
        volume.set_len(channels);
        for (index, output) in volume.get_mut().iter_mut().enumerate() {
            *output = volume_from_percent(values.get(index).copied().unwrap_or(fallback));
        }

        self.set_sink_volume(name, volume, current)
    }

    fn set_sink_db(&mut self, name: &str, db: f64) -> Result<SinkState> {
        let current = self.current_or_query(name)?;
        let channels = current.volumes_percent.len().max(1);
        let channels =
            u8::try_from(channels).map_err(|_| anyhow!("too many PulseAudio channels"))?;
        let mut volume = ChannelVolumes::default();
        volume.set(channels, Volume::from(VolumeDB(db)));
        self.set_sink_volume(name, volume, current)
    }

    fn set_sink_mute(&mut self, name: &str, muted: bool) -> Result<SinkState> {
        let mut state = self.current_or_query(name)?;
        if state.muted == muted {
            return Ok(state);
        }

        let success = Rc::new(RefCell::new(None));
        let callback_success = success.clone();
        let mut introspector = self.context.introspect();
        let operation = introspector.set_sink_mute_by_name(
            name,
            muted,
            Some(Box::new(move |ok| {
                *callback_success.borrow_mut() = Some(ok);
            })),
        );

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if success.borrow().as_ref().copied() != Some(true) {
            bail!("PulseAudio rejected sink mute update for {name}");
        }

        state.muted = muted;
        self.update_cache(&state);
        Ok(state)
    }

    fn set_sink_input_percent(&mut self, index: u32, percent: f64) -> Result<StreamState> {
        let mut state = self.query_sink_input(index)?;
        if !state.has_volume || !state.volume_writable {
            bail!("PulseAudio sink input {index} does not expose writable volume");
        }

        let channels = state.volumes_percent.len().max(1);
        let channels =
            u8::try_from(channels).map_err(|_| anyhow!("too many PulseAudio channels"))?;
        let mut volume = ChannelVolumes::default();
        volume.set(channels, volume_from_percent(percent));

        let success = Rc::new(RefCell::new(None));
        let callback_success = success.clone();
        let mut introspector = self.context.introspect();
        let operation = introspector.set_sink_input_volume(
            index,
            &volume,
            Some(Box::new(move |ok| {
                *callback_success.borrow_mut() = Some(ok);
            })),
        );

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if success.borrow().as_ref().copied() != Some(true) {
            bail!("PulseAudio rejected sink-input volume update for {index}");
        }

        state.volumes_percent = volume
            .get()
            .iter()
            .copied()
            .map(percent_from_volume)
            .collect();
        Ok(state)
    }

    fn set_sink_input_mute(&mut self, index: u32, muted: bool) -> Result<StreamState> {
        let mut state = self.query_sink_input(index)?;
        if state.muted == muted {
            return Ok(state);
        }

        let success = Rc::new(RefCell::new(None));
        let callback_success = success.clone();
        let mut introspector = self.context.introspect();
        let operation = introspector.set_sink_input_mute(
            index,
            muted,
            Some(Box::new(move |ok| {
                *callback_success.borrow_mut() = Some(ok);
            })),
        );

        wait_for_operation(&mut self.mainloop, &self.context, &operation)?;
        if success.borrow().as_ref().copied() != Some(true) {
            bail!("PulseAudio rejected sink-input mute update for {index}");
        }

        state.muted = muted;
        Ok(state)
    }

    fn pump(&mut self) -> Result<()> {
        iterate_once(&mut self.mainloop, &self.context)?;

        let events = self.events.borrow_mut().drain(..).collect::<Vec<_>>();
        if events.is_empty() {
            return Ok(());
        }

        for event in events {
            if let PulseEvent::Sink(operation, index) = event {
                if operation == Some(SubscribeOperation::Removed) {
                    self.remove_cached_index(index);
                } else if let Err(err) = self.query_sink_by_index(index) {
                    debug!(
                        index,
                        error = %err,
                        "PulseAudio sink subscription refresh failed"
                    );
                }
            }
        }
        self.notify_changes();
        Ok(())
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::GetSink { name, reply } => {
                let _ = reply.send(self.current_or_query(&name));
            }
            Request::ListSinks { reply } => {
                let _ = reply.send(self.list_sinks());
            }
            Request::ListSinkInputs { reply } => {
                let _ = reply.send(self.list_sink_inputs());
            }
            Request::SetSinkPercent {
                name,
                values,
                reply,
            } => {
                let _ = reply.send(self.set_sink_percent(&name, &values));
            }
            Request::SetSinkDb { name, db, reply } => {
                let _ = reply.send(self.set_sink_db(&name, db));
            }
            Request::SetSinkMute { name, muted, reply } => {
                let _ = reply.send(self.set_sink_mute(&name, muted));
            }
            Request::SetSinkInputPercent {
                index,
                percent,
                reply,
            } => {
                let _ = reply.send(self.set_sink_input_percent(index, percent));
            }
            Request::SetSinkInputMute {
                index,
                muted,
                reply,
            } => {
                let _ = reply.send(self.set_sink_input_mute(index, muted));
            }
        }
    }
}

fn reject_request(request: Request, message: &str) {
    match request {
        Request::GetSink { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::ListSinks { reply } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::ListSinkInputs { reply } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::SetSinkPercent { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::SetSinkDb { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::SetSinkMute { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::SetSinkInputPercent { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
        Request::SetSinkInputMute { reply, .. } => {
            let _ = reply.send(Err(anyhow!(message.to_owned())));
        }
    }
}

fn clear_cache(cache: &RwLock<HashMap<String, SinkState>>) {
    if let Ok(mut state) = cache.write() {
        state.clear();
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<Request>,
    cache: Arc<RwLock<HashMap<String, SinkState>>>,
    changes: watch::Sender<u64>,
) {
    let mut connection = None;
    let mut next_reconnect = Instant::now();

    loop {
        if connection.is_none() && Instant::now() >= next_reconnect {
            match PulseConnection::connect(cache.clone(), changes.clone()) {
                Ok(new_connection) => {
                    connection = Some(new_connection);
                    changes.send_modify(|generation| *generation = generation.wrapping_add(1));
                }
                Err(err) => {
                    debug!(error = %err, "Persistent PulseAudio connection unavailable");
                    next_reconnect = Instant::now() + RECONNECT_INTERVAL;
                }
            }
        }

        if let Some(active) = connection.as_mut() {
            if let Err(err) = active.pump() {
                debug!(error = %err, "Persistent PulseAudio connection lost");
                clear_cache(&cache);
                connection = None;
                changes.send_modify(|generation| *generation = generation.wrapping_add(1));
                next_reconnect = Instant::now() + RECONNECT_INTERVAL;
            }
        }

        let wait = if connection.is_some() {
            IDLE_INTERVAL
        } else {
            next_reconnect.saturating_duration_since(Instant::now())
        };

        match receiver.recv_timeout(wait) {
            Ok(request) => {
                if let Some(active) = connection.as_mut() {
                    active.handle(request);
                    if !active.healthy() {
                        clear_cache(&cache);
                        connection = None;
                        changes.send_modify(|generation| *generation = generation.wrapping_add(1));
                        next_reconnect = Instant::now() + RECONNECT_INTERVAL;
                    }
                } else {
                    reject_request(request, "PulseAudio server is unavailable");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
