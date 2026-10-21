import type { FoundAccount } from "../api/types";
import { DiscoverHot } from "../components/Discover";
import { Sheet } from "../components/ui";

/** After a restore: on-chain accounts this recovery phrase has used before. */
export function DiscoverSheet({ found }: { found: FoundAccount[] }) {
  return (
    <Sheet title="Accounts found">
      <p className="notice">
        This recovery phrase has used {found.length === 1 ? "an on-chain account" : `${found.length} on-chain accounts`} before. Add {found.length === 1 ? "it" : "them"} to see the balance and history. Adding turns on Advanced mode.
      </p>
      <DiscoverHot initial={found} />
    </Sheet>
  );
}
