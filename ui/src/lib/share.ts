import { copyText } from "./clipboard";
import { t } from "./i18n.svelte";
import type { Toasts } from "./toasts.svelte";

/// Canonical TIDAL web links for the Share action.
export type ShareKind = "playlist" | "album" | "mix" | "artist" | "track";

export function shareUrl(kind: ShareKind, id: string | number): string {
  return `https://tidal.com/${kind}/${id}`;
}

/// Copy the item's TIDAL link and say whether it worked. Returns the outcome
/// so callers without a toast host can still react.
export async function copyShareLink(kind: ShareKind, id: string | number, toasts?: Toasts): Promise<boolean> {
  const copied = await copyText(shareUrl(kind, id));
  toasts?.show(copied ? t("card.linkCopied") : t("card.linkCopyFailed"), {
    severity: copied ? "success" : "error",
  });
  return copied;
}
