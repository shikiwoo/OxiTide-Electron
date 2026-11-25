// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { offsetWithin, scrollParent, scrolledPast } from "./pageScroll";

afterEach(() => {
  document.body.innerHTML = "";
});

function rectAt(top: number): DOMRect {
  return { top, bottom: top, left: 0, right: 0, width: 0, height: 0, x: 0, y: top, toJSON: () => ({}) } as DOMRect;
}

describe("scrollParent", () => {
  it("finds the nearest vertically scrolling ancestor", () => {
    document.body.innerHTML = `
      <div id="outer" style="overflow-y:auto"><div id="mid" style="overflow:hidden">
        <div id="inner" style="overflow-y:scroll"><div><span id="leaf"></span></div></div>
      </div></div>`;
    expect(scrollParent(document.getElementById("leaf")!)?.id).toBe("inner");
    expect(scrollParent(document.getElementById("inner")!)?.id).toBe("outer");
  });

  it("is null when nothing above scrolls", () => {
    document.body.innerHTML = `<div><span id="leaf"></span></div>`;
    expect(scrollParent(document.getElementById("leaf")!)).toBeNull();
  });
});

describe("offsetWithin / scrolledPast", () => {
  it("measures from the top of the host's content, whatever its scroll", () => {
    const host = document.createElement("div");
    const node = document.createElement("div");
    host.appendChild(node);
    host.getBoundingClientRect = () => rectAt(20);
    // The node sits 300px into the content; the host is scrolled by 120.
    host.scrollTop = 120;
    node.getBoundingClientRect = () => rectAt(20 + 300 - host.scrollTop);
    expect(offsetWithin(host, node)).toBe(300);
    expect(scrolledPast(host, node)).toBe(0);
    host.scrollTop = 450;
    expect(offsetWithin(host, node)).toBe(300);
    expect(scrolledPast(host, node)).toBe(150);
  });
});
