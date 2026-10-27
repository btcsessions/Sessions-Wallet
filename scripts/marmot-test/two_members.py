"""Two Marmot accounts over public relays, used the way the wallet plugin uses them."""
import asyncio, os, secrets, sys, tempfile, threading, time
sys.path.insert(0, sys.argv[1])
import marmot_uniffi as mk

RELAYS = ["ws://127.0.0.1:7771", "ws://127.0.0.1:7772"]
CHARSET = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"

def bech32(hrp, data):
    def polymod(v):
        g = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3]; c = 1
        for x in v:
            b = c >> 25; c = ((c & 0x1ffffff) << 5) ^ x
            for i in range(5):
                if (b >> i) & 1: c ^= g[i]
        return c
    acc = bits = 0; words = []
    for byte in data:
        acc = (acc << 8) | byte; bits += 8
        while bits >= 5: bits -= 5; words.append((acc >> bits) & 31)
    if bits: words.append((acc << (5 - bits)) & 31)
    exp = [ord(c) >> 5 for c in hrp] + [0] + [ord(c) & 31 for c in hrp]
    mod = polymod(exp + words + [0] * 6) ^ 1
    return hrp + "1" + "".join(CHARSET[w] for w in words + [(mod >> 5 * (5 - i)) & 31 for i in range(6)])

class Store(mk.SecretStore):
    def __init__(self): self.by_id = {}; self.labels = {}; self.lock = threading.Lock()
    def has_secret_for_label(self, label):
        with self.lock: return self.labels.get(label) in self.by_id
    def has_secret_for_account_id(self, account_id_hex):
        with self.lock: return account_id_hex in self.by_id
    def write_secret(self, label, account_id_hex, secret_key_hex):
        with self.lock: self.labels[label] = account_id_hex; self.by_id[account_id_hex] = secret_key_hex
    def load_secret(self, label, account_id_hex):
        with self.lock:
            k = self.by_id.get(account_id_hex or self.labels.get(label, ""))
        if k is None: raise mk.MarmotKitError.SecretNotFound("none")
        return k
    def remove_secret(self, label, account_id_hex):
        with self.lock: self.labels.pop(label, None)

async def reopen(root, sk_hex, acct_hex):
    st = Store(); st.by_id[acct_hex] = sk_hex
    opts = mk.MarmotOptions(relay_policy=mk.RelayPolicyFfi.ALLOW_LOOPBACK, cursor_persistence=None, client_name="Sessions Wallet test",
                            secret_store=st, attachment_acquisition_mode=None, moderation_report_config=None,
                            deployment=mk.marmot_deployment_neutral())
    m = mk.Marmot.new_with_configuration(root, RELAYS, opts)
    await m.start()
    known = [a.account_id_hex for a in m.list_accounts()]
    print("reopen: known", acct_hex in known)
    try:
        await m.sign_in_account(acct_hex)
    except Exception as e:
        print("sign in:", type(e).__name__, e)
    return m

ROOTS = {}
async def make(name):
    root = tempfile.mkdtemp(prefix=f"marmot-{name}-")
    opts = mk.MarmotOptions(relay_policy=mk.RelayPolicyFfi.ALLOW_LOOPBACK, cursor_persistence=None, client_name="Sessions Wallet test",
                            secret_store=Store(), attachment_acquisition_mode=None, moderation_report_config=None,
                            deployment=mk.marmot_deployment_neutral())
    m = mk.Marmot.new_with_configuration(root, RELAYS, opts)
    await m.start()
    sk = secrets.token_bytes(32)
    acct = await m.login(bech32("nsec", sk), RELAYS, RELAYS, RELAYS)
    ROOTS[name] = (root, sk.hex(), acct.account_id_hex)
    print(name, "account", acct.account_id_hex[:12], "label", acct.label)
    return m, acct.account_id_hex

async def wait(what, f, timeout=90):
    t = time.time()
    while time.time() - t < timeout:
        r = await f()
        if r: print("ok:", what, f"({time.time()-t:.0f}s)"); return r
        await asyncio.sleep(2)
    raise SystemExit(f"timed out: {what}")

async def main():
    alice, A = await make("alice")
    bob, B = await make("bob")
    gid = await alice.create_group_with_options(A, "Test vault", [], mk.CreateGroupOptionsFfi(description=None, initial_image=None, disappearing_message_secs=0))
    print("group", gid[:12])
    async def invite():
        try:
            await alice.invite_members(A, gid, [B]); return True
        except Exception as e:
            print("  invite:", e); return False
    await wait("invite (bob's key package found)", invite)
    async def pending():
        rows = bob.chat_list(B, True)
        return [r for r in rows if r.group_id_hex == gid and r.pending_confirmation]
    await wait("bob sees invite", pending)
    d = await bob.group_details(B, gid)
    print("welcomer is alice:", d.group.welcomer_account_id_hex == A)
    await bob.accept_group_invite(B, gid)
    await alice.send_custom_event(A, gid, 4601, [], '{"v":1,"id":"p1"}')
    async def bob_got():
        return [m for m in bob.messages(B, gid, 100, [4601])]
    got = await wait("bob gets wallet message", bob_got)
    print("sender is alice:", got[0].sender == A, "content:", got[0].plaintext)
    await bob.send_custom_event(B, gid, 4602, [], '{"v":1,"slot":1}')
    async def alice_got():
        return [m for m in alice.messages(A, gid, 100, [4602])]
    got = await wait("alice gets bob's message", alice_got)
    print("sender is bob:", got[0].sender == B)
    mem = await alice.group_members(A, gid)
    print("members:", sorted(x.member_id_hex[:8] for x in mem), "A", A[:8], "B", B[:8])
    print("admins:", [a[:8] for a in (await alice.group_details(A, gid)).group.admins])
    await alice.send_text(A, gid, "hello")
    async def chat(): return [m for m in bob.messages(B, gid, 100, [9])]
    await wait("chat", chat)
    await alice.shutdown_and_close()
    root, skh, ah = ROOTS["alice"]
    alice = await reopen(root, skh, A)
    await bob.send_custom_event(B, gid, 4604, [], '{"v":1,"after":"restart"}')
    async def after(): return [m for m in alice.messages(A, gid, 100, [4604])]
    await wait("alice after restart gets new message", after)
    print("old messages kept:", len(alice.messages(A, gid, 100, [4601, 4602])))
    await alice.shutdown_and_close(); await bob.shutdown_and_close()
    print("ALL OK")

asyncio.run(main())
