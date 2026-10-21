// Trezor Safe 7 through Trezor Connect 10, which routes every request through
// the Trezor Suite app:
//   • desktop: Suite listens on ws://127.0.0.1:21335 and handles USB/Bluetooth,
//     pairing (THP) and passphrase itself;
//   • Android/iOS: we open a Connect deep link into Trezor Suite, which calls
//     back into this app at sessionswallet://trezor.
// Trezor firmware doesn't sign PSBTs, so the Rust side converts our PSBT to
// signTransaction params and folds the signed transaction back in.

import { api } from "../api";

export const isMobile = () => /android|iphone|ipad/i.test(navigator.userAgent);

const MANIFEST = {
  appName: "Sessions Wallet",
  appUrl: "https://github.com/btcsessions/sessions-wallet",
  email: "",
};

const CALLBACK = "sessionswallet://trezor";

type Failure = { success: false; payload?: { error?: string; code?: string }; error?: unknown };
type Result<T> = { success: true; payload: T } | Failure;

interface Connect {
  getPublicKey(p: Record<string, unknown>): Promise<Result<{ xpub: string; descriptor?: string; rootFingerprint?: number; serializedPath: string }>>;
  signTransaction(p: Record<string, unknown>): Promise<Result<{ serializedTx: string }>>;
  getAddress(p: Record<string, unknown>): Promise<Result<{ address: string }>>;
}

let ready: Promise<Connect> | null = null;

async function connect(): Promise<Connect> {
  ready ??= (async () => {
    if (isMobile()) {
      const [{ default: TC }, { openUrl }, { onOpenUrl }] = await Promise.all([
        import("@trezor/connect-mobile"),
        import("@tauri-apps/plugin-opener"),
        import("@tauri-apps/plugin-deep-link"),
      ]);
      await TC.init({
        manifest: MANIFEST,
        deeplinkOpen: (url: string) => void openUrl(url),
        deeplinkCallbackUrl: CALLBACK,
      });
      await onOpenUrl((urls) => urls.filter((u) => u.startsWith(CALLBACK)).forEach((u) => TC.handleDeeplink(u)));
      return TC as unknown as Connect;
    }
    // Talk to Trezor Suite desktop directly. (The packaged client falls back
    // to a Suite Web popup when Suite isn't running, which can't work here.)
    const [{ CoreInSuiteDesktop }, { VERSION }] = await Promise.all([
      import("@trezor/connect-web/lib/impl/core-in-suite-desktop.js"),
      import("@trezor/connect-common/lib/data/version.js"),
    ]);
    const core = new CoreInSuiteDesktop();
    await core.init({ manifest: MANIFEST, version: VERSION } as Parameters<typeof core.init>[0]);
    const call = (method: string) => (p: Record<string, unknown>) => core.call({ method, ...p } as Parameters<typeof core.call>[0]);
    return {
      getPublicKey: call("getPublicKey"),
      signTransaction: call("signTransaction"),
      getAddress: call("getAddress"),
    } as unknown as Connect;
  })().catch((e) => {
    ready = null;
    throw new Error(explain(String(e?.message ?? e), e?.code));
  });
  return ready;
}

function explain(error: string, code?: string): string {
  if (code === "Desktop_ConnectionMissing" || /websocket|connection/i.test(error)) {
    return "Couldn't reach Trezor Suite. Open Trezor Suite (version 26.7 or later) on this computer, unlock your Trezor, then try again.";
  }
  if (/cancel/i.test(error)) return "Cancelled on the Trezor.";
  return error;
}

function unwrap<T>(r: Result<T>): T {
  if (r.success) return r.payload;
  const raw = r.payload?.error ?? r.error;
  const msg = typeof raw === "string" ? raw : raw instanceof Error ? raw.message : String((raw as { message?: string })?.message ?? "Trezor Suite didn't respond");
  const code = r.payload?.code ?? (raw as { code?: string })?.code;
  throw new Error(explain(msg, code));
}

export type TrezorScript = "wpkh" | "tr";

const PURPOSE: Record<TrezorScript, number> = { wpkh: 84, tr: 86 };

/** The account's descriptor, read from the device via Trezor Suite. */
export async function trezorDescriptor(script: TrezorScript, testnet = false, account = 0): Promise<string> {
  const tc = await connect();
  const path = `m/${PURPOSE[script]}'/${testnet ? 1 : 0}'/${account}'`;
  const r = unwrap(
    await tc.getPublicKey({
      path,
      coin: testnet ? "test" : "btc",
      scriptType: script === "tr" ? "SPENDTAPROOT" : "SPENDWITNESS",
    }),
  );
  if (r.descriptor) return r.descriptor;
  if (r.rootFingerprint === undefined) throw new Error("Trezor Suite didn't return the wallet fingerprint. Update Trezor Suite and try again.");
  const fp = (r.rootFingerprint >>> 0).toString(16).padStart(8, "0");
  const origin = path.slice(2).replace(/'/g, "h");
  return `${script}([${fp}/${origin}]${r.xpub}/<0;1>/*)`;
}

/** Sign an unsigned PSBT on the Trezor. Returns the PSBT with the signed inputs. */
export async function trezorSign(psbt: string): Promise<string> {
  const a = await api();
  const params = await a.trezorSignRequest(psbt);
  const tc = await connect();
  const signed = unwrap(await tc.signTransaction(params));
  return a.trezorMerge(psbt, signed.serializedTx);
}
