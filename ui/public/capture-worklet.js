// Collects input audio into interleaved chunks and posts them to the page.
// Each message carries the context frame of the chunk's first sample, so the
// page can line engine time up with the audio clock.
const CHUNK = 256;

class Capture extends AudioWorkletProcessor {
  constructor() {
    super();
    this.channels = 0;
    this.buf = null;
    this.fill = 0;
    this.start = 0;
  }

  process(inputs) {
    const input = inputs[0];
    const n = input && input.length;
    if (!n) return true;
    if (n !== this.channels) {
      this.channels = n;
      this.buf = new Float32Array(CHUNK * n);
      this.fill = 0;
    }
    const frames = input[0].length;
    for (let i = 0; i < frames; i++) {
      if (this.fill === 0) this.start = currentFrame + i;
      const base = this.fill * n;
      for (let c = 0; c < n; c++) this.buf[base + c] = input[c][i];
      this.fill++;
      if (this.fill === CHUNK) {
        this.port.postMessage({ frame: this.start, channels: n, data: this.buf }, [this.buf.buffer]);
        this.buf = new Float32Array(CHUNK * n);
        this.fill = 0;
      }
    }
    return true;
  }
}

registerProcessor("capture", Capture);
