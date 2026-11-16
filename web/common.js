// Partes da página do telinha que ficam num arquivo à parte: identificação de
// quem assiste e o decodificador do modo túnel/relé (WebCodecs).

// localStorage pode não existir (janela anônima, armazenamento bloqueado).
function stored(key, fallback) {
  try { return localStorage.getItem(key) ?? fallback; } catch { return fallback; }
}
function store(key, value) {
  try { localStorage.setItem(key, value); } catch {}
}

// Id aleatório deste navegador: permite que quem transmite bloqueie alguém.
const clientId = (() => {
  let id = stored('telinha-client', '');
  if (!id) {
    id = (crypto.randomUUID?.() ?? Math.random().toString(36).slice(2) + Date.now().toString(36));
    store('telinha-client', id);
  }
  return id;
})();

// O navegador não revela o nome do computador; mostramos navegador · sistema.
function deviceName() {
  const ua = navigator.userAgent;
  const browser = /Edg\//.test(ua) ? 'Edge' : /OPR\//.test(ua) ? 'Opera' : /Firefox\//.test(ua) ? 'Firefox'
    : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'Navegador';
  const os = /Android/.test(ua) ? 'Android' : /iPhone|iPad/.test(ua) ? 'iOS' : /Windows/.test(ua) ? 'Windows'
    : /Mac OS X/.test(ua) ? 'macOS' : /CrOS/.test(ua) ? 'ChromeOS' : /Linux/.test(ua) ? 'Linux' : '';
  return os ? `${browser} · ${os}` : browser;
}

// Resumo dos candidatos ICE de uma linha SDP "candidate:": tipo e família do endereço.
function candidateKind(line) {
  const parts = line.split(' ');
  const address = parts[4] || '';
  const type = parts[parts.indexOf('typ') + 1] || '?';
  const family = address.endsWith('.local') ? 'mDNS' : address.includes(':') ? 'IPv6' : 'IPv4';
  return `${type}/${family}`;
}

const tunnelSupported = () => 'VideoDecoder' in window && 'EncodedVideoChunk' in window;

// Tipos de NAL de um quadro Annex B (diagnóstico): 1 = fatia, 5 = IDR, 6 = SEI, 7 = SPS, 8 = PPS, 9 = AUD.
function nalTypes(data) {
  const types = [];
  for (let i = 0; i + 3 < data.length && types.length < 8; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && (data[i + 2] === 1 || (data[i + 2] === 0 && data[i + 3] === 1))) {
      const at = data[i + 2] === 1 ? i + 3 : i + 4;
      types.push(data[at] & 0x1f);
      i = at;
    }
  }
  return types;
}

// Limpa um quadro Annex B: tira o delimitador (AUD) e mantém só a primeira
// cópia de SPS/PPS (o encoder e o h264parse podem repetir; o Firefox recusa).
function cleanAccessUnit(data) {
  const units = [];
  let start = -1;
  for (let i = 0; i + 2 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      if (start >= 0) units.push(data.subarray(start, i > 0 && data[i - 1] === 0 ? i - 1 : i));
      start = i + 3;
      i += 2;
    }
  }
  if (start >= 0) units.push(data.subarray(start));
  const seen = new Set();
  const kept = units.filter((u) => {
    const type = u[0] & 0x1f;
    if (type === 9) return false;
    if (type === 7 || type === 8) {
      if (seen.has(type)) return false;
      seen.add(type);
    }
    return u.length > 0;
  });
  const out = new Uint8Array(kept.reduce((n, u) => n + 4 + u.length, 0));
  let at = 0;
  for (const u of kept) { out.set([0, 0, 0, 1], at); out.set(u, at + 4); at += 4 + u.length; }
  return out;
}

// Quadros sem nenhuma fatia de imagem (só SPS/PPS, às vezes marcados como
// "delta") derrubam o decodificador. Eles ficam guardados e vão junto do
// próximo quadro. Devolve o quadro a decodificar, ou null para pular.
function withHeaders(state, data) {
  const types = nalTypes(data);
  if (!types.includes(1) && !types.includes(5)) {
    if (types.includes(7) || types.includes(8)) state.headers = data;
    return null;
  }
  if (state.headers) {
    const joined = new Uint8Array(state.headers.length + data.length);
    joined.set(state.headers);
    joined.set(data, state.headers.length);
    state.headers = null;
    return joined;
  }
  return data;
}

async function pickVideoCodec() {
  const forced = new URLSearchParams(location.search).get('codec');
  if (forced) return forced;
  // H.264 constrained baseline; o nível só limita a resolução declarada.
  for (const codec of ['avc1.42E034', 'avc1.42E02A', 'avc1.42E01F']) {
    try {
      if ((await VideoDecoder.isConfigSupported({ codec, optimizeForLatency: true })).supported) return codec;
    } catch {}
  }
  return null;
}

// Toca um fluxo do relé (jam): quadros H.264 já codificados desenhados num
// canvas e pacotes Opus tocados pelo Web Audio. `kind`: 1 = quadro-chave,
// 2 = quadro, 3 = áudio. Opções: onFrame(primeiro?), requestKey(), log(texto).
class RelayPlayer {
  constructor(canvas, audioCtx, opts) {
    this.canvas = canvas;
    this.ctx2d = canvas.getContext('2d');
    this.audioCtx = audioCtx;
    this.opts = opts;
    this.codec = '';
    this.decoder = null;
    this.audio = null;
    this.gain = null;
    this.volume = 0;
    this.playhead = 0;
    this.waitingKey = true;
    this.frames = 0;
    this.bytes = 0;
    this.skipped = 0;
    this.closed = false;
    this.ready = pickVideoCodec().then((codec) => {
      this.codec = codec || '';
      if (!codec) opts.log?.('relé: nenhum H.264 suportado pelo WebCodecs');
    });
  }

  newDecoder() {
    return new VideoDecoder({
      output: (frame) => {
        if (this.closed) return frame.close();
        if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
          this.canvas.width = frame.displayWidth;
          this.canvas.height = frame.displayHeight;
        }
        this.ctx2d.drawImage(frame, 0, 0);
        frame.close();
        this.frames++;
        this.opts.onFrame?.(this.frames === 1);
      },
      error: (err) => {
        this.opts.log?.(`relé: erro no decodificador (NALs ${this.lastNals}): ${err.message}`);
        this.waitingKey = true;
        this.decoder = null;
      },
    });
  }

  feed(kind, ts, data, byteLength) {
    if (this.closed) return;
    this.bytes += byteLength;
    if (kind === 3) return this.feedAudio(ts, data);
    data = withHeaders(this, data);
    if (!data) return;
    const key = kind === 1;
    if (!this.decoder || this.decoder.state === 'closed') {
      if (!this.codec) return;
      this.decoder = this.newDecoder();
      this.decoder.configure({ codec: this.codec, optimizeForLatency: true });
      this.waitingKey = true;
    }
    // Computador sem fôlego para decodificar: pula até o próximo quadro-chave.
    if (this.decoder.decodeQueueSize > 8) this.waitingKey = true;
    if (this.waitingKey && !key) {
      this.skipped++;
      const now = performance.now();
      if (!this.lastKeyRequest || now - this.lastKeyRequest > 1000) {
        this.lastKeyRequest = now;
        this.opts.requestKey?.();
      }
      return;
    }
    this.waitingKey = false;
    this.lastNals = `${key ? 'chave' : 'delta'} ${nalTypes(data).join(',')}`;
    this.decoder.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data: cleanAccessUnit(data) }));
  }

  feedAudio(ts, data) {
    const ctx = this.audioCtx;
    if (!ctx || !('AudioDecoder' in window)) return;
    if (!this.audio) {
      this.gain = ctx.createGain();
      this.gain.gain.value = this.volume;
      this.gain.connect(ctx.destination);
      this.audio = new AudioDecoder({
        output: (audioData) => {
          if (this.closed) return audioData.close();
          const buffer = ctx.createBuffer(audioData.numberOfChannels, audioData.numberOfFrames, audioData.sampleRate);
          for (let ch = 0; ch < audioData.numberOfChannels; ch++) {
            const plane = new Float32Array(audioData.numberOfFrames);
            audioData.copyTo(plane, { planeIndex: ch, format: 'f32-planar' });
            buffer.copyToChannel(plane, ch);
          }
          audioData.close();
          // Pequena reserva (80 ms) contra engasgos; se acumular atraso, recomeça.
          const now = ctx.currentTime;
          if (this.playhead < now + 0.02 || this.playhead > now + 0.4) this.playhead = now + 0.08;
          const source = ctx.createBufferSource();
          source.buffer = buffer;
          source.connect(this.gain);
          source.start(this.playhead);
          this.playhead += buffer.duration;
        },
        error: (err) => { this.opts.log?.(`relé: erro no áudio: ${err.message}`); this.audio = null; },
      });
      this.audio.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
    }
    if (this.audio.state === 'configured') this.audio.decode(new EncodedAudioChunk({ type: 'key', timestamp: ts, data }));
  }

  setVolume(v) {
    this.volume = v;
    if (this.gain) this.gain.gain.value = v;
  }

  close() {
    this.closed = true;
    try { this.decoder?.close(); } catch {}
    try { this.audio?.close(); } catch {}
    try { this.gain?.disconnect(); } catch {}
  }
}
