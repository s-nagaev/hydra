// Admin UI helpers, self-hosted like admin.css and htmx.min.js: no build
// step, no dependencies, embedded into the binary with include_bytes!.
//
// The log detail page shows captured bodies inside <pre class="code-snippet">
// elements. A body is attacker-influenced input, so the highlighter reads
// the element with textContent, tokenizes the JSON in that string, and
// rebuilds the element out of text nodes and <span> elements. Raw body text
// never becomes an HTML string.
(function () {
  "use strict";

  // One JSON token: a string (a key when a colon follows it, group 2), a
  // literal, a number, or a piece of punctuation. Everything between
  // matches is copied verbatim as plain text.
  var TOKEN = /("(?:\\u[0-9a-fA-F]{4}|\\[^u]|[^\\"\u0000-\u001f])*")(\s*:)?|\b(true|false|null)\b|(-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?)|([{}\[\],:])/g;

  function token(className, text) {
    var node = document.createElement("span");
    node.className = className;
    node.textContent = text;
    return node;
  }

  // Turns one JSON document into a fragment of styled text nodes.
  function highlightTokens(text) {
    var fragment = document.createDocumentFragment();
    var cursor = 0;
    var match;
    TOKEN.lastIndex = 0;
    while ((match = TOKEN.exec(text)) !== null) {
      if (match.index > cursor) {
        fragment.appendChild(document.createTextNode(text.slice(cursor, match.index)));
      }
      if (match[1] !== undefined) {
        if (match[2] !== undefined) {
          fragment.appendChild(token("json-key", match[1]));
          fragment.appendChild(token("json-punct", match[2]));
        } else {
          fragment.appendChild(token("json-string", match[1]));
        }
      } else if (match[3] !== undefined) {
        fragment.appendChild(token("json-literal", match[3]));
      } else if (match[4] !== undefined) {
        fragment.appendChild(token("json-number", match[4]));
      } else if (match[5] !== undefined) {
        fragment.appendChild(token("json-punct", match[5]));
      }
      cursor = TOKEN.lastIndex;
    }
    if (cursor < text.length) {
      fragment.appendChild(document.createTextNode(text.slice(cursor)));
    }
    return fragment;
  }

  // Not JSON (an SSE capture, a truncated body, an error page): the stored
  // text stays exactly as the server rendered it.
  function highlight(pre) {
    var value;
    try {
      value = JSON.parse(pre.textContent);
    } catch (error) {
      return;
    }
    pre.textContent = "";
    pre.appendChild(highlightTokens(JSON.stringify(value, null, 2)));
  }

  function run() {
    var bodies = document.querySelectorAll("pre.code-snippet");
    for (var index = 0; index < bodies.length; index++) {
      highlight(bodies[index]);
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();
