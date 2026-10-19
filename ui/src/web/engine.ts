/** The engine compiled to WebAssembly, driven through its plain exports. */
export interface EngineEvent {
  time: number;
  dest: number;
  data: number[];
}

interface Exports {
  memory: WebAssembly.Memory;
  configure(sampleRate: number, channels: number): void;
  input_buffer(samples: number): number;
  process(samples: number): number;
  take_events(): number;
  events_ptr(): number;
  rpc_buffer(len: number): number;
  rpc(): number;
  rpc_result(): number;
  sample_time(): number;
  persist_version(): number;
}

export class WasmEngine {
  private x: Exports;
  private enc = new TextEncoder();
  private dec = new TextDecoder();

  private constructor(x: Exports) {
    this.x = x;
  }

  static async load(url: string): Promise<WasmEngine> {
    const res = await fetch(url);
    if (!res.ok) throw new Error(`Could not load the engine (${res.status})`);
    const { instance } = await WebAssembly.instantiate(await res.arrayBuffer(), {});
    return new WasmEngine(instance.exports as unknown as Exports);
  }

  configure(sampleRate: number, channels: number) {
    this.x.configure(sampleRate, channels);
  }

  process(interleaved: Float32Array): EngineEvent[] {
    const p = this.x.input_buffer(interleaved.length);
    new Float32Array(this.x.memory.buffer, p, interleaved.length).set(interleaved);
    return this.readEvents(this.x.process(interleaved.length));
  }

  takeEvents(): EngineEvent[] {
    return this.readEvents(this.x.take_events());
  }

  private readEvents(n: number): EngineEvent[] {
    if (n === 0) return [];
    const v = new Float64Array(this.x.memory.buffer, this.x.events_ptr(), n * 4);
    const out: EngineEvent[] = [];
    for (let i = 0; i < n; i++) {
      const len = v[i * 4 + 2];
      const packed = v[i * 4 + 3];
      const data = [packed & 255, (packed >> 8) & 255, (packed >> 16) & 255].slice(0, len);
      out.push({ time: v[i * 4], dest: v[i * 4 + 1], data });
    }
    return out;
  }

  rpc(request: unknown): any {
    const bytes = this.enc.encode(JSON.stringify(request));
    const p = this.x.rpc_buffer(bytes.length);
    new Uint8Array(this.x.memory.buffer, p, bytes.length).set(bytes);
    const n = this.x.rpc();
    const out = new Uint8Array(this.x.memory.buffer, this.x.rpc_result(), n);
    return JSON.parse(this.dec.decode(out));
  }

  sampleTime(): number {
    return this.x.sample_time();
  }

  persistVersion(): number {
    return this.x.persist_version();
  }
}
