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

type PublicKey = {
  xpub: string;
  descriptor?: string;
  rootFingerprint?: number;
  /** Parent fingerprint of this key (the root's, for a depth-1 key). */
  fingerprint?: number;
  depth?: number;
  serializedPath: string;
};

interface Connect {
  getPublicKey(p: Record<string, unknown>): Promise<Result<PublicKey>>;
  signTransaction(p: Record<string, unknown>): Promise<Result<{ serializedTx: string; signatures?: (string | null | undefined)[] }>>;
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

/** Descriptors for several account numbers in one Trezor Suite request. */
export async function trezorDescriptors(script: TrezorScript, accounts: number[]): Promise<string[]> {
  const tc = await connect();
  const paths = accounts.map((a) => `m/${PURPOSE[script]}'/0'/${a}'`);
  const rs = unwrap(
    await tc.getPublicKey({
      bundle: paths.map((path) => ({ path, coin: "btc", scriptType: script === "tr" ? "SPENDTAPROOT" : "SPENDWITNESS" })),
    }),
  ) as unknown as Array<{ descriptor?: string; xpub: string; rootFingerprint?: number }>;
  return rs.map((r, i) => {
    if (r.descriptor) return r.descriptor;
    if (r.rootFingerprint === undefined) throw new Error("Trezor Suite didn't return the wallet fingerprint. Update Trezor Suite and try again.");
    const fp = (r.rootFingerprint >>> 0).toString(16).padStart(8, "0");
    return `${script}([${fp}/${paths[i].slice(2).replace(/'/g, "h")}]${r.xpub}/<0;1>/*)`;
  });
}

/** Sign an unsigned PSBT on the Trezor. Returns the PSBT with the signed inputs. */
export async function trezorSign(psbt: string): Promise<string> {
  const a = await api();
  const params = await a.trezorSignRequest(psbt);
  const tc = await connect();
  const signed = unwrap(await tc.signTransaction(params));
  return a.trezorMerge(psbt, signed.serializedTx);
}

// --- multisig vaults ---------------------------------------------------------
//
// Vault keys are BIP48 native SegWit (m/48'/coin'/account'/2'). Signing and
// showing addresses name the whole quorum; sorted (sortedmulti) vaults need
// Trezor firmware 2.8.7+ (Trezor One 1.13.0+). The Rust side builds the
// Connect parameters for this Trezor's root fingerprint and checks what comes
// back.

const hex8 = (n: number) => (n >>> 0).toString(16).padStart(8, "0");

/** The root fingerprint, from a key Trezor Suite returned (or by asking for m/48'). */
async function rootFingerprintOf(tc: Connect, key: PublicKey): Promise<string> {
  if (key.rootFingerprint !== undefined) return hex8(key.rootFingerprint);
  // Older Trezor Suite: a depth-1 key's parent fingerprint is the root's.
  const top = await tc.getPublicKey({ path: "m/48'" }).catch(() => null);
  if (top?.success && top.payload.depth === 1 && top.payload.fingerprint !== undefined) return hex8(top.payload.fingerprint);
  throw new Error("Trezor Suite didn't return the wallet fingerprint. Update Trezor Suite and try again.");
}

async function multisigAccountKey(tc: Connect, account: number, testnet: boolean): Promise<{ key: PublicKey; path: string }> {
  const path = `m/48'/${testnet ? 1 : 0}'/${account}'/2'`;
  const key = unwrap(
    await tc.getPublicKey({
      path,
      coin: testnet ? "test" : "btc",
      scriptType: "SPENDWITNESS",
      ignoreXpubMagic: true,
    }),
  );
  return { key, path };
}

/**
 * Root fingerprint (8 hex characters) of the Trezor in use, for the vault
 * requests. It's the same on every network, so this asks for a mainnet key.
 */
export async function trezorRootFingerprint(): Promise<string> {
  const tc = await connect();
  const { key } = await multisigAccountKey(tc, 0, false);
  return rootFingerprintOf(tc, key);
}

/** This Trezor's key for a multisig vault: `[fingerprint/48h/0h/{account}h/2h]xpub…` (plain xpub/tpub). */
export async function trezorMultisigKey(account = 0, testnet = false): Promise<string> {
  const tc = await connect();
  const { key, path } = await multisigAccountKey(tc, account, testnet);
  if (!/^[xt]pub/.test(key.xpub)) throw new Error("Trezor Suite returned the key in an unexpected format. Update Trezor Suite and try again.");
  const fp = await rootFingerprintOf(tc, key);
  return `[${fp}/${path.slice(2).replace(/'/g, "h")}]${key.xpub}`;
}

/** The vault commands on the Rust side (registered in lib.rs; see src-tauri/src/multisig_hw.rs). */
async function vaultCommand<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  const { invoke } = await import("@tauri-apps/api/core");
  return invoke<T>(cmd, args);
}

/**
 * Sign a PSBT spending from multisig vault `account` on the Trezor. Returns
 * the PSBT with the Trezor's signatures added, not finalized: other
 * cosigners may still need to sign. `fingerprint` (the Trezor's root
 * fingerprint, if known) saves asking Trezor Suite for it.
 */
export async function trezorMultisigSign(account: string, psbt: string, fingerprint?: string): Promise<string> {
  const fp = fingerprint ?? (await trezorRootFingerprint());
  const params = await vaultCommand<Record<string, unknown>>("trezor_multisig_sign_request", { account, psbt, fingerprint: fp });
  const tc = await connect();
  const signed = unwrap(await tc.signTransaction(params));
  // One per input; Connect leaves holes where the device didn't sign.
  const count = Array.isArray(params.inputs) ? params.inputs.length : (signed.signatures?.length ?? 0);
  const signatures = Array.from({ length: count }, (_, i) => signed.signatures?.[i] || null);
  return vaultCommand<string>("trezor_multisig_merge", { psbt, signatures, signedTx: signed.serializedTx });
}

/**
 * Show receive address `index` of multisig vault `account` on the Trezor,
 * after the wallet worked out which address it must be. Returns the address
 * once the Trezor showed it.
 */
export async function trezorShowMultisigAddress(account: string, index: number, fingerprint?: string): Promise<string> {
  const fp = fingerprint ?? (await trezorRootFingerprint());
  const { params, address } = await vaultCommand<{ params: Record<string, unknown>; address: string }>(
    "trezor_multisig_address_request",
    { account, index, fingerprint: fp },
  );
  const tc = await connect();
  // Passing `address` makes Connect check it as well.
  const shown = unwrap(await tc.getAddress({ ...params, address }));
  if (shown.address !== address) throw new Error("The Trezor showed a different address than this wallet. Don't use it");
  return shown.address;
}
