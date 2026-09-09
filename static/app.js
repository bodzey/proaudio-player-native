const $ = (selector) => document.querySelector(selector);
const controls = [...document.querySelectorAll("button[data-action]"), $("#volume"), $("#mute")];
let state = null;
let library = [];
let queue = [];
let volumeTimer = null;
let toastTimer = null;

async function api(path, options = {}) {
  const response = await fetch(path, {
    headers: { "Content-Type": "application/json", ...(options.headers || {}) },
    ...options,
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || `HTTP ${response.status}`);
  return data;
}

function toast(message, error = false) {
  const element = $("#toast");
  element.textContent = message;
  element.className = `toast show${error ? " error" : ""}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { element.className = "toast"; }, 3200);
}

function externalPlayerActive() {
  const player = state?.player;
  if (player) {
    return !["none", "mpd"].includes(player.backend) && ["playing", "paused"].includes(player.state);
  }
  return Boolean(state?.sources?.some((source) => source.type !== "Локальна бібліотека"));
}

function setLocked(locked) {
  controls.forEach((control) => { control.disabled = locked; });
  $("#refresh-library").disabled = locked;
  $("#clear-queue").disabled = locked || queue.length === 0;
  document.querySelectorAll(".item").forEach((item) => { item.disabled = locked; });
  document.querySelectorAll(".queue-play, .queue-remove").forEach((button) => {
    button.disabled = locked || (button.classList.contains("queue-play") && externalPlayerActive());
  });
}

function transportNote(player) {
  if (player.backend === "spotify-mpris") return "Керування активним Spotify Connect через MPRIS.";
  if (player.backend === "airplay-mpris") return "Керування AirPlay залежить від підтримки remote control на пристрої-джерелі.";
  if (player.backend === "mpd") return "Кнопки керують локальним MPD-плеєром.";
  if (player.backend === "external") return "Активне джерело не надає доступного керування треками.";
  return "Керування з’явиться після запуску підтримуваного джерела.";
}

function renderTransport(player, priorityActive) {
  const play = $("#play");
  play.dataset.action = player.state === "playing" ? "pause" : "play";
  play.textContent = player.state === "playing" ? "❚❚" : "▶";
  play.title = player.state === "playing" ? "Пауза" : "Відтворити";

  document.querySelectorAll("button[data-action]").forEach((button) => {
    const action = button.dataset.action;
    button.disabled = priorityActive || !Boolean(player.controls?.[action]);
  });
  $(".transport-note").textContent = transportNote(player);
}

function fallbackPlayer(data) {
  const mpd = data.mpd || {};
  const external = data.sources?.find((source) => source.type !== "Локальна бібліотека");
  if (external) {
    return {
      source: external.type,
      backend: "external",
      state: "playing",
      title: external.media || "Аудіопотік",
      artist: external.application || "",
      album: "",
      progress: 0,
      elapsed: null,
      duration: null,
      controls: {},
    };
  }
  const localActive = ["playing", "paused"].includes(mpd.state);
  return {
    source: localActive ? "Локальна бібліотека" : "Немає потоку",
    backend: localActive ? "mpd" : "none",
    state: mpd.state || "stopped",
    title: localActive ? mpd.title : "",
    artist: localActive ? mpd.artist : "",
    album: localActive ? mpd.album : "",
    progress: mpd.progress || 0,
    elapsed: mpd.elapsed,
    duration: mpd.duration,
    controls: localActive ? { play: true, pause: true, stop: true, next: true, prev: true } : {},
  };
}

function renderStatus(data) {
  state = data;
  const connection = $("#connection");
  connection.className = "badge online";
  connection.innerHTML = "<span></span>В мережі";

  const priority = data.priority || {};
  $("#priority").classList.toggle("hidden", !priority.active);
  if (priority.active) {
    $("#priority-title").textContent = priority.minute_silence_active
      ? "Хвилина мовчання"
      : "Повітряна тривога";
    $("#priority-text").textContent = "Пріоритетне оповіщення активне. Музичні елементи заблоковано.";
  }

  const volume = Math.max(0, Math.min(100, Math.round(data.volume || 0)));
  $("#volume").value = volume;
  $("#volume-value").textContent = volume;
  $("#mute").textContent = data.muted ? "🔇" : "🔊";
  $("#mute").title = data.muted ? "Увімкнути звук" : "Вимкнути звук";

  const levels = data.audio_levels || {};
  if (levels.physical) {
    $("#master-level").value = levels.physical.volume;
    $("#master-level-value").textContent = `${Math.round(levels.physical.volume)}%`;
  } else {
    $("#master-level-value").textContent = "—";
  }
  if (levels.alert_bus) {
    $("#alert-bus-level").value = levels.alert_bus.volume;
    $("#alert-bus-level-value").textContent = `${Math.round(levels.alert_bus.volume)}%`;
  } else {
    $("#alert-bus-level-value").textContent = "—";
  }

  const player = data.player || fallbackPlayer(data);
  const hasPlayer = player.backend !== "none";
  $("#source-chip").textContent = player.source || "Немає потоку";
  $("#track-title").textContent = hasPlayer
    ? (player.title || "Аудіопотік")
    : "Очікування аудіо";
  const metadata = [player.artist, player.album].filter(Boolean).join(" • ");
  $("#track-meta").textContent = metadata
    || (hasPlayer ? player.source : "Оберіть ProAudio Player у Spotify, AirPlay, DLNA або запустіть локальний трек.");

  const showProgress = hasPlayer && Boolean(player.duration);
  $("#track-progress").classList.toggle("hidden", !showProgress);
  $("#progress-fill").style.width = `${Math.max(0, Math.min(100, player.progress || 0))}%`;
  $("#elapsed").textContent = player.elapsed || "0:00";
  $("#duration").textContent = player.duration || "0:00";

  setLocked(Boolean(priority.active));
  renderTransport(player, Boolean(priority.active));
  updateQueueState();
  $("#last-update").textContent = `Оновлено ${new Date().toLocaleTimeString("uk-UA")}`;
}

async function refreshStatus() {
  try {
    renderStatus(await api("/api/status"));
  } catch (error) {
    const connection = $("#connection");
    connection.className = "badge error";
    connection.innerHTML = "<span></span>Немає зв’язку";
  }
}

function renderItems(container, items, kind) {
  container.innerHTML = "";
  if (!items.length) {
    container.innerHTML = `<p class="empty">${kind === "track" ? "Локальних треків немає" : "Збережених плейлістів немає"}</p>`;
    return;
  }
  items.forEach((value) => {
    const button = document.createElement("button");
    button.className = "item";
    button.disabled = Boolean(state?.priority?.active);
    const label = document.createElement("span");
    label.textContent = value;
    const action = document.createElement("b");
    action.textContent = "ВІДТВОРИТИ";
    button.append(label, action);
    button.addEventListener("click", async () => {
      try {
        const path = kind === "track" ? "/api/library/play" : "/api/playlists/load";
        const key = kind === "track" ? "path" : "name";
        await api(path, { method: "POST", body: JSON.stringify({ [key]: value }) });
        toast("Відтворення запущено");
        await Promise.all([refreshStatus(), loadQueue()]);
      } catch (error) { toast(error.message, true); }
    });
    container.append(button);
  });
}

function renderQueue(items) {
  const container = $("#queue");
  $("#queue-count").textContent = items.length;
  $("#clear-queue").disabled = Boolean(state?.priority?.active) || items.length === 0;
  container.innerHTML = "";
  if (!items.length) {
    container.innerHTML = '<p class="empty">Черга порожня</p>';
    return;
  }
  items.forEach((item) => {
    const row = document.createElement("div");
    row.className = `queue-item${item.position === state?.mpd?.queue_position ? " current" : ""}`;
    row.dataset.position = item.position;

    const play = document.createElement("button");
    play.className = "queue-play";
    play.disabled = Boolean(state?.priority?.active);
    const title = document.createElement("span");
    title.className = "queue-title";
    title.textContent = item.title || item.file;
    const meta = document.createElement("small");
    meta.textContent = [item.artist, item.album].filter(Boolean).join(" • ") || item.file;
    play.append(title, meta);
    play.addEventListener("click", async () => {
      try {
        await api("/api/queue/play", { method: "POST", body: JSON.stringify({ position: item.position }) });
        await refreshStatus();
      } catch (error) { toast(error.message, true); }
    });

    const remove = document.createElement("button");
    remove.className = "queue-remove";
    remove.title = "Вилучити з черги";
    remove.textContent = "×";
    remove.disabled = Boolean(state?.priority?.active);
    remove.addEventListener("click", async () => {
      try {
        await api("/api/queue/remove", { method: "POST", body: JSON.stringify({ position: item.position }) });
        await Promise.all([loadQueue(), refreshStatus()]);
      } catch (error) { toast(error.message, true); }
    });
    row.append(play, remove);
    container.append(row);
  });
}

function updateQueueState() {
  document.querySelectorAll(".queue-item").forEach((row) => {
    row.classList.toggle("current", Number(row.dataset.position) === state?.mpd?.queue_position);
  });
}

async function loadQueue() {
  try {
    queue = (await api("/api/queue")).items || [];
    renderQueue(queue);
  } catch (error) { toast(error.message, true); }
}

async function loadLibrary() {
  try {
    const [tracks, playlists] = await Promise.all([api("/api/library"), api("/api/playlists")]);
    library = tracks.items || [];
    renderItems($("#library"), library, "track");
    renderItems($("#playlists"), playlists.items || [], "playlist");
  } catch (error) { toast(error.message, true); }
}

function alertSettingsPayload() {
  const token = $("#alerts-token").value.trim();
  const payload = {
    endpoint: $("#alerts-endpoint").value.trim(),
    location_uid: Number($("#alerts-location-uid").value),
    location_type: $("#alerts-location-type").value,
    poll_interval_seconds: Number($("#alerts-poll").value),
    request_timeout_seconds: Number($("#alerts-timeout").value),
    rate_limit_backoff_seconds: Number($("#alerts-backoff").value),
    clear_confirmations: Number($("#alerts-clear-confirmations").value),
  };
  if (token) payload.token = token;
  return payload;
}

function renderAlertSettings(data) {
  $("#alerts-endpoint").value = data.endpoint;
  $("#alerts-location-uid").value = data.location_uid;
  $("#alerts-location-type").value = data.location_type;
  $("#alerts-poll").value = data.poll_interval_seconds;
  $("#alerts-timeout").value = data.request_timeout_seconds;
  $("#alerts-backoff").value = data.rate_limit_backoff_seconds;
  $("#alerts-clear-confirmations").value = data.clear_confirmations;
  $("#alerts-token").value = "";
  $("#alerts-token-state").textContent = data.token_configured
    ? "Токен налаштовано"
    : "Токен відсутній";
}

function settingsResult(message, error = false) {
  const result = $("#alerts-settings-result");
  result.textContent = message;
  result.className = `settings-result${error ? " error" : " success"}`;
}

async function loadAlertSettings() {
  try {
    renderAlertSettings(await api("/api/settings/alerts"));
  } catch (error) {
    settingsResult(error.message, true);
  }
}

function renderAudioSettings(data) {
  $("#audio-duck-db").value = data.duck_db;
  $("#audio-alert-volume").value = data.alert_volume_percent;
  $("#audio-minute-volume").value = data.minute_silence_volume_percent;
  $("#audio-restore-volume").value = data.default_restore_volume_percent;
  $("#audio-duck-fade").value = data.duck_fade_seconds;
  $("#audio-restore-fade").value = data.restore_fade_seconds;
}

async function loadAudioSettings() {
  try {
    renderAudioSettings(await api("/api/settings/audio"));
  } catch (error) { toast(error.message, true); }
}

function formatHardwareLevel(item) {
  const db = Number.isFinite(item.db) ? ` / ${item.db > 0 ? "+" : ""}${item.db.toFixed(2)} dB` : "";
  return `${item.volume}%${db}`;
}

async function loadAudioOutputs() {
  const select = $("#audio-output");
  const status = $("#audio-output-status");
  try {
    const items = (await api("/api/audio/outputs")).items || [];
    const previous = select.value;
    select.innerHTML = "";
    if (!items.length) {
      select.innerHTML = '<option value="">Фізичних аудіовиходів не знайдено</option>';
      select.disabled = true;
      status.textContent = "Підключіть USB DAC або активуйте вбудований аудіовихід.";
      return;
    }
    items.forEach((item) => {
      const option = document.createElement("option");
      option.value = item.id;
      option.textContent = `${item.name} · ${item.state}`;
      option.selected = item.selected;
      select.append(option);
    });
    if (!items.some((item) => item.selected) && items.some((item) => item.id === previous)) select.value = previous;
    select.disabled = Boolean(state?.priority?.active);
    const active = items.find((item) => item.id === select.value);
    status.textContent = active ? `Активний вихід: ${active.name}` : "Оберіть фізичний аудіовихід.";
  } catch (error) {
    select.innerHTML = '<option value="">Помилка отримання аудіовиходів</option>';
    select.disabled = true;
    status.textContent = error.message;
  }
}

async function loadHardwareMixers() {
  const container = $("#hardware-mixers");
  try {
    const items = (await api("/api/audio/hardware")).items || [];
    container.innerHTML = "";
    if (!items.length) {
      container.innerHTML = '<p class="empty">Апаратних ALSA-регуляторів відтворення не знайдено.</p>';
      return;
    }
    items.forEach((item) => {
      const label = document.createElement("label");
      label.className = "level-control hardware-control";
      const title = document.createElement("span");
      const value = document.createElement("b");
      value.textContent = formatHardwareLevel(item);
      title.append(`${item.card_name}: ${item.control} `, value);
      const slider = document.createElement("input");
      slider.type = "range";
      slider.min = "0";
      slider.max = "100";
      slider.step = "1";
      slider.value = item.volume;
      slider.addEventListener("change", async () => {
        try {
          await api("/api/audio/hardware", {
            method: "POST",
            body: JSON.stringify({ card: item.card, control: item.control, percent: Number(slider.value) }),
          });
          await loadHardwareMixers();
          toast("Апаратний ALSA-рівень змінено");
        } catch (error) { toast(error.message, true); }
      });
      const note = document.createElement("small");
      const limits = Number.isInteger(item.raw_min) && Number.isInteger(item.raw_max)
        ? ` ALSA raw: ${item.raw_min}…${item.raw_max}.`
        : "";
      note.textContent = `DEV: фактичний апаратний мікшер; значення зчитується з пристрою.${limits}`;
      label.append(title, slider, note);
      container.append(label);
    });
  } catch (error) {
    container.innerHTML = `<p class="empty">${error.message}</p>`;
  }
}

function bindLevel(id, valueId, target) {
  const slider = $(id);
  slider.addEventListener("input", () => { $(valueId).textContent = `${slider.value}%`; });
  slider.addEventListener("change", async () => {
    try {
      await api("/api/audio/level", {
        method: "POST",
        body: JSON.stringify({ target, percent: Number(slider.value) }),
      });
      await refreshStatus();
    } catch (error) { toast(error.message, true); }
  });
}

$("#audio-output").addEventListener("change", async (event) => {
  const id = event.target.value;
  if (!id) return;
  event.target.disabled = true;
  $("#audio-output-status").textContent = "Перемикання аудіовиходу…";
  try {
    await api("/api/audio/outputs", { method: "POST", body: JSON.stringify({ id }) });
    toast("Аудіовихід збережено. Перебудова аудіошин…");
    setTimeout(async () => {
      await Promise.all([loadAudioOutputs(), loadHardwareMixers(), refreshStatus()]);
    }, 1800);
  } catch (error) {
    toast(error.message, true);
    await loadAudioOutputs();
  }
});

$("#refresh-audio-outputs").addEventListener("click", async () => {
  await Promise.all([loadAudioOutputs(), loadHardwareMixers()]);
});

bindLevel("#master-level", "#master-level-value", "master");
bindLevel("#alert-bus-level", "#alert-bus-level-value", "alert");

$("#audio-settings").addEventListener("submit", async (event) => {
  event.preventDefault();
  const result = $("#audio-settings-result");
  try {
    const data = await api("/api/settings/audio", {
      method: "PUT",
      body: JSON.stringify({
        duck_db: Number($("#audio-duck-db").value),
        alert_volume_percent: Number($("#audio-alert-volume").value),
        minute_silence_volume_percent: Number($("#audio-minute-volume").value),
        default_restore_volume_percent: Number($("#audio-restore-volume").value),
        duck_fade_seconds: Number($("#audio-duck-fade").value),
        restore_fade_seconds: Number($("#audio-restore-fade").value),
      }),
    });
    renderAudioSettings(data);
    result.textContent = "Збережено";
    result.className = "settings-result success";
  } catch (error) {
    result.textContent = error.message;
    result.className = "settings-result error";
  }
});

$("#stream-player").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = event.submitter;
  const result = $("#stream-result");
  if (button) button.disabled = true;
  result.textContent = "Підключення…";
  result.className = "settings-result";
  try {
    await api("/api/streams/play", {
      method: "POST",
      body: JSON.stringify({ url: $("#stream-url").value.trim() }),
    });
    result.textContent = "Потік запущено";
    result.className = "settings-result success";
    toast("Мережевий потік запущено");
    setTimeout(refreshStatus, 250);
  } catch (error) {
    result.textContent = error.message;
    result.className = "settings-result error";
  } finally {
    if (button) button.disabled = false;
  }
});

$("#alerts-settings").addEventListener("submit", async (event) => {
  event.preventDefault();
  const button = event.submitter;
  if (button) button.disabled = true;
  settingsResult("Збереження…");
  try {
    renderAlertSettings(await api("/api/settings/alerts", {
      method: "PUT",
      body: JSON.stringify(alertSettingsPayload()),
    }));
    settingsResult("Налаштування збережено");
    toast("Налаштування API збережено");
  } catch (error) {
    settingsResult(error.message, true);
  } finally {
    if (button) button.disabled = false;
  }
});

$("#test-alerts-settings").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  settingsResult("Перевірка API…");
  try {
    const result = await api("/api/settings/alerts/test", {
      method: "POST",
      body: JSON.stringify(alertSettingsPayload()),
    });
    settingsResult(result.active
      ? "API працює: для локації зараз активна тривога"
      : "API працює: для локації зараз немає тривоги");
  } catch (error) {
    settingsResult(error.message, true);
  } finally {
    button.disabled = false;
  }
});

document.querySelectorAll("button[data-action]").forEach((button) => {
  button.addEventListener("click", async () => {
    try {
      await api("/api/player", {
        method: "POST",
        body: JSON.stringify({ action: button.dataset.action }),
      });
      setTimeout(refreshStatus, 150);
    } catch (error) { toast(error.message, true); }
  });
});

$("#volume").addEventListener("input", (event) => {
  $("#volume-value").textContent = event.target.value;
  clearTimeout(volumeTimer);
  volumeTimer = setTimeout(async () => {
    try {
      await api("/api/volume", { method: "POST", body: JSON.stringify({ percent: Number(event.target.value) }) });
      await refreshStatus();
    } catch (error) { toast(error.message, true); }
  }, 180);
});

$("#mute").addEventListener("click", async () => {
  try {
    await api("/api/mute", { method: "POST", body: JSON.stringify({ muted: !state?.muted }) });
    await refreshStatus();
  } catch (error) { toast(error.message, true); }
});

$("#search").addEventListener("input", (event) => {
  const query = event.target.value.trim().toLocaleLowerCase("uk-UA");
  renderItems($("#library"), library.filter((item) => item.toLocaleLowerCase("uk-UA").includes(query)), "track");
});

$("#refresh-library").addEventListener("click", async () => {
  try {
    await api("/api/library/update", { method: "POST" });
    toast("Оновлення бібліотеки запущено");
    setTimeout(loadLibrary, 1800);
  } catch (error) { toast(error.message, true); }
});

$("#clear-queue").addEventListener("click", async () => {
  try {
    await api("/api/queue/clear", { method: "POST" });
    await Promise.all([loadQueue(), refreshStatus()]);
  } catch (error) { toast(error.message, true); }
});

refreshStatus();
loadLibrary();
loadQueue();
loadAlertSettings();
loadAudioSettings();
loadAudioOutputs();
loadHardwareMixers();
setInterval(refreshStatus, 3000);
setInterval(loadAudioOutputs, 15000);
