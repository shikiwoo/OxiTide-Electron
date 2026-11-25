/// Helpers for content that scrolls with its page instead of in its own box
/// (TrackTable's `fill` mode): the page's scroller drives it, and positions
/// inside the content are measured from where it starts on that scroller.

/// The nearest ancestor that scrolls vertically (`overflow-y: auto|scroll`),
/// or null when none does.
export function scrollParent(node: HTMLElement): HTMLElement | null {
  for (let el = node.parentElement; el; el = el.parentElement) {
    const overflow = getComputedStyle(el).overflowY;
    if (overflow === "auto" || overflow === "scroll" || overflow === "overlay") return el;
  }
  return null;
}

/// Where `node` starts in `host`'s scrolled content, in px from the top of
/// that content (stable while `host` scrolls).
export function offsetWithin(host: HTMLElement, node: HTMLElement): number {
  return node.getBoundingClientRect().top - host.getBoundingClientRect().top - host.clientTop + host.scrollTop;
}

/// How far `host` has scrolled past the top of `node` (0 while `node`'s top
/// is still in view).
export function scrolledPast(host: HTMLElement, node: HTMLElement): number {
  return Math.max(0, host.scrollTop - offsetWithin(host, node));
}
