/** Human-readable name of a control source descriptor (matches the engine). */
export function describeSource(source: string): string {
  const p = source.split(":");
  if (p[0] === "midi" && p.length === 4) {
    const kind = p[1] === "note" ? "note" : p[1] === "cc" ? "CC" : "program";
    return `MIDI ${kind} ${p[3]} (ch ${p[2]})`;
  }
  if (p[0] === "pad") return `Gamepad ${p[1]}`;
  if (p[0] === "key") return `Key ${p[1].replace(/^Key/, "").replace(/^Digit/, "")}`;
  return source;
}
