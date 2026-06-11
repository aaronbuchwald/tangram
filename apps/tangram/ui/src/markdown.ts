// Markdown rendering: marked -> HTML, then DOMPurify -> safe HTML.
//
// Per ADR-0007 / the shell redesign (Decision B), both libraries are real
// npm dependencies bundled by Vite (not vendored single files) — the shell
// is the one app with a build step. The markdown comes from the user's own
// replicated document, but a sync peer could author it, so sanitizing before
// `innerHTML` is mandatory defense-in-depth.

import DOMPurify from "dompurify";
import { marked } from "marked";

marked.setOptions({ gfm: true, breaks: false });

export function renderMarkdown(source: string): string {
  const raw = marked.parse(source, { async: false }) as string;
  return DOMPurify.sanitize(raw);
}
