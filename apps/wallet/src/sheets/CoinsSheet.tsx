import { CoinList } from "../components/Coins";
import { Sheet } from "../components/ui";
import { useWallet } from "../state/store";

/** Every coin in an on-chain account, with labels and freezing (Advanced). */
export function CoinsSheet({ account }: { account: string }) {
  const { accounts, open } = useWallet();
  const a = accounts.find((x) => x.id === account);
  return (
    <Sheet
      title="Coins"
      left={
        <button className="text-button" onClick={() => open({ name: "account", id: account })}>
          Back
        </button>
      }
    >
      <p className="group-note" style={{ margin: "0 4px 14px" }}>
        Each coin in {a?.name ?? "this account"}. Tap one to label it or freeze it. Labels from receive addresses and sends carry over to their coins.
      </p>
      <CoinList account={account} />
    </Sheet>
  );
}
