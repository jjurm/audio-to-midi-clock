import { invoke } from "@tauri-apps/api/core";
import type { AudioDevice, Backend, HostStatus } from "./types";

/** Backend for the desktop app: the engine and all I/O run in Rust. */
export async function createDesktopBackend(): Promise<Backend> {
  return {
    kind: "desktop",
    needsGesture: false,
    async start() {
      await invoke("start_host");
    },
    async rpc(request) {
      const res = await invoke<string>("rpc", { request: JSON.stringify(request) });
      return JSON.parse(res);
    },
    listAudioDevices: () => invoke<AudioDevice[]>("list_audio_devices"),
    hostStatus: () => invoke<HostStatus>("host_status"),
  };
}
