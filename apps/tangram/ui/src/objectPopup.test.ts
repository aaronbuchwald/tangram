// DOM tests for the basic object popup (Smart Objects SO1): it opens, edits
// type/data/links, Save calls `onSave` with parsed links, Delete calls
// `onDelete`, and the links-text parser round-trips.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  isObjectPopupOpen,
  openObjectPopup,
  parseLinksText,
} from "./objectPopup";
import type { ObjectType, SmartObject } from "./api";

const TYPES: ObjectType[] = [
  { name: "note-ref", label: "Note reference", render: "chip" },
  { name: "tag", label: "Tag", render: "chip" },
];

const obj = (over: Partial<SmartObject> = {}): SmartObject => ({
  id: "o1",
  type: "tag",
  data: "hello",
  links: [{ rel: "references", target: "o2" }],
  render: "chip",
  ...over,
});

afterEach(() => {
  // Ensure no popup leaks across tests.
  document.querySelectorAll(".modal-overlay").forEach((n) => n.remove());
});

describe("parseLinksText", () => {
  it("parses `rel target [url]` lines, dropping blanks/incomplete", () => {
    const text = "references o2\nsee https://x  \nbad\n\ncites o9 https://doi";
    expect(parseLinksText(text)).toEqual([
      { rel: "references", target: "o2" },
      { rel: "see", target: "https://x" },
      { rel: "cites", target: "o9", url: "https://doi" },
    ]);
  });
});

describe("object popup (DOM)", () => {
  it("opens single-instance and reports open state", () => {
    expect(isObjectPopupOpen()).toBe(false);
    openObjectPopup(obj(), {
      onSave: () => {},
      onDelete: () => {},
      onClose: () => {},
      objectTypes: () => TYPES,
    });
    expect(isObjectPopupOpen()).toBe(true);
    expect(document.querySelectorAll(".object-popup").length).toBe(1);
    // A second open dismisses the first (single-instance).
    openObjectPopup(obj({ id: "o2" }), {
      onSave: () => {},
      onDelete: () => {},
      onClose: () => {},
      objectTypes: () => TYPES,
    });
    expect(document.querySelectorAll(".object-popup").length).toBe(1);
  });

  it("Save passes the edited type/data/parsed links to onSave", () => {
    const onSave = vi.fn();
    openObjectPopup(obj(), {
      onSave,
      onDelete: () => {},
      onClose: () => {},
      objectTypes: () => TYPES,
    });
    const dialog = document.querySelector(".object-popup")!;
    const type = dialog.querySelector<HTMLSelectElement>(".object-popup-type")!;
    const data = dialog.querySelector<HTMLTextAreaElement>(".object-popup-data")!;
    const links = dialog.querySelector<HTMLTextAreaElement>(".object-popup-links")!;
    expect(type.value).toBe("tag");
    expect(data.value).toBe("hello");
    type.value = "note-ref";
    data.value = "{}";
    links.value = "references o9";
    dialog.querySelector<HTMLButtonElement>(".object-popup-save")!.click();
    expect(onSave).toHaveBeenCalledWith(
      "note-ref",
      "{}",
      [{ rel: "references", target: "o9" }],
      "chip",
    );
    expect(isObjectPopupOpen()).toBe(false);
  });

  it("Delete calls onDelete and closes", () => {
    const onDelete = vi.fn();
    openObjectPopup(obj(), {
      onSave: () => {},
      onDelete,
      onClose: () => {},
      objectTypes: () => TYPES,
    });
    document
      .querySelector<HTMLButtonElement>(".object-popup-delete")!
      .click();
    expect(onDelete).toHaveBeenCalledTimes(1);
    expect(isObjectPopupOpen()).toBe(false);
  });

  it("surfaces an unregistered type as a selectable `(custom)` option", () => {
    openObjectPopup(obj({ type: "recipe" }), {
      onSave: () => {},
      onDelete: () => {},
      onClose: () => {},
      objectTypes: () => TYPES,
    });
    const type = document.querySelector<HTMLSelectElement>(".object-popup-type")!;
    expect(type.value).toBe("recipe");
    expect([...type.options].some((o) => o.textContent?.includes("custom"))).toBe(true);
  });
});
