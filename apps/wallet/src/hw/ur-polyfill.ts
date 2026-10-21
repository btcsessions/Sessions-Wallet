// bc-ur expects Node's Buffer as a global. Imported before it, so it's set first.
import { Buffer } from "buffer";

const g = globalThis as unknown as { Buffer?: typeof Buffer };
if (!g.Buffer) g.Buffer = Buffer;
