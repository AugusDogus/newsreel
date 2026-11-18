import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

/** The pair page the engine serves to a device it does not know, which is not part of this app. */
const PAGE = process.env.PAIR_PAGE ?? new URL("../../crates/daemon/src/daemon/web/pair.html", import.meta.url).pathname;

type Listener = (event: FakeEvent) => void;
interface FakeEvent {
  key?: string;
  keyCode?: number;
  prevented: boolean;
  preventDefault(): void;
}

class FakeElement {
  value = "";
  disabled = false;
  textContent = "";
  className = "";
  listeners: Record<string, Listener[]> = {};
  constructor(
    readonly id: string,
    readonly page: FakePage,
  ) {}
  addEventListener(name: string, listener: Listener) {
    (this.listeners[name] ??= []).push(listener);
  }
  focus() {
    this.page.activeElement = this;
  }
  select() {}
  fire(name: string, event: FakeEvent) {
    for (const listener of this.listeners[name] ?? []) listener(event);
  }
}

class FakePage {
  activeElement: FakeElement | null = null;
  posted: string[] = [];
  readonly elements = new Map(["pair", "code", "go", "status"].map((id) => [id, new FakeElement(id, this)]));
  readonly document = new FakeElement("document", this);
  element(id: string): FakeElement {
    return this.elements.get(id)!;
  }
  /** A key pressed on the remote: it goes to the element with focus, then to the document. */
  press(key: string, keyCode = 0): FakeEvent {
    const event = { key, keyCode, prevented: false, preventDefault: () => void (event.prevented = true) };
    this.activeElement?.fire("keydown", event);
    if (!event.prevented && key === "Enter" && this.activeElement?.id === "go") this.element("pair").fire("submit", event);
    this.document.fire("keydown", event);
    return event;
  }
}

function open(): FakePage {
  const page = new FakePage();
  const html = readFileSync(PAGE, "utf8");
  const script = html.slice(html.indexOf("<script>") + 8, html.lastIndexOf("</script>"));
  const document = Object.defineProperties(page.document, {
    getElementById: { value: (id: string) => page.element(id) },
    activeElement: { get: () => page.activeElement },
  });
  class XMLHttpRequest {
    open() {}
    setRequestHeader() {}
    send(body: string) {
      page.posted.push(body);
    }
  }
  const location = { hash: "", pathname: "/", replace() {} };
  new Function("document", "XMLHttpRequest", "location", "history", script)(document, XMLHttpRequest, location, {});
  page.element("code").focus();
  return page;
}

test("a remote reaches Pair from the code with Down and back with Up", () => {
  const page = open();
  expect(page.press("ArrowDown").prevented).toBe(true);
  expect(page.activeElement?.id).toBe("go");
  page.press("ArrowUp");
  expect(page.activeElement?.id).toBe("code");
  // Some TV browsers send only the key code.
  page.press("Unidentified", 40);
  expect(page.activeElement?.id).toBe("go");
});

test("OK on the code pairs once it has six digits, and the sixth digit moves to Pair", () => {
  const page = open();
  const code = page.element("code");
  code.value = "12345";
  page.press("Enter");
  expect(page.posted).toEqual([]);
  code.value = "123456";
  code.fire("input", { prevented: false, preventDefault() {} });
  expect(page.activeElement?.id).toBe("go");
  page.element("code").focus();
  page.press("Enter");
  expect(page.posted).toEqual(['{"code":"123456"}']);
});
