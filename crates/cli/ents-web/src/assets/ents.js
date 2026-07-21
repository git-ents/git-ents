/*
 * The shell's `⌘K` shortcut: focus the top bar's `.palette` search input
 * (the `kbd` hint every page renders beside it -- `crate::pages`'s
 * `layout_shell`). Cmd+K on macOS, Ctrl+K elsewhere; Escape hands focus
 * back. Enhancement only: the form is a plain GET to `/search` with or
 * without this handler.
 */
(function () {
  "use strict";

  var input = document.querySelector('.palette input[name="q"]');
  if (!input) {
    return;
  }
  document.addEventListener("keydown", function (event) {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      input.focus();
      input.select();
    } else if (event.key === "Escape" && document.activeElement === input) {
      input.blur();
    }
  });
})();

/*
 * Progressive enhancement for `crate::pages::files`'s raw-source blob view
 * (`div.blob[data-path][data-rev]`): click a gutter line number to select
 * it, shift-click to extend the selection to a range, and open an inline
 * comment composer cloned from the server-rendered
 * `<template id="composer-template">`. Every behavior here layers on top
 * of markup that already works with no script at all -- the plain `#L<n>`
 * anchors and the header's "comment on this file" link -- so a disabled or
 * failed script load never breaks the page, only the shortcut.
 *
 * Vanilla, dependency-free: no fetch/AJAX anywhere in this file. The
 * composer's own submit is left as an ordinary form POST; only its Cancel
 * button and the gutter's "+" affordance are wired up here.
 */
(function () {
  "use strict";

  var blob = document.querySelector("div.blob[data-path][data-rev]");
  if (!blob) {
    return;
  }
  var table = blob.querySelector("table");
  if (!table) {
    return;
  }

  var rows = Array.prototype.slice.call(table.querySelectorAll("tbody > tr"));

  function lineNumber(tr) {
    var a = tr.querySelector("td.blob-nums a");
    return a ? parseInt(a.textContent, 10) : null;
  }

  var lineRows = rows.filter(function (tr) {
    return lineNumber(tr) !== null;
  });
  var byNumber = {};
  lineRows.forEach(function (tr) {
    byNumber[lineNumber(tr)] = tr;
  });

  var anchorLine = null;

  // The blob header's open-in-editor deep link follows the selection:
  // `data-editor-base` carries the line-less URL, the selected line is
  // appended in the editors' shared `:{line}` suffix shape.
  var editorLink = blob.querySelector("a.editor-open[data-editor-base]");
  function retargetEditor(line) {
    if (editorLink) {
      editorLink.href = editorLink.getAttribute("data-editor-base") + ":" + line;
    }
  }

  function selectRange(start, end) {
    retargetEditor(Math.min(start, end));
    var lo = Math.min(start, end);
    var hi = Math.max(start, end);
    lineRows.forEach(function (tr) {
      tr.classList.remove("sel");
    });
    for (var n = lo; n <= hi; n += 1) {
      if (byNumber[n]) {
        byNumber[n].classList.add("sel");
      }
    }
  }

  function setHash(start, end) {
    var hash = start === end ? "#L" + start : "#L" + start + "-L" + end;
    history.replaceState(null, "", hash);
  }

  function applyHash(hash, scroll) {
    var match = /^#L(\d+)(?:-L(\d+))?$/.exec(hash);
    if (!match) {
      return;
    }
    var start = parseInt(match[1], 10);
    var end = match[2] ? parseInt(match[2], 10) : start;
    anchorLine = start;
    selectRange(start, end);
    if (scroll && byNumber[start] && byNumber[start].scrollIntoView) {
      byNumber[start].scrollIntoView({ block: "center" });
    }
  }

  if (location.hash) {
    applyHash(location.hash, true);
  }

  table.querySelectorAll("td.blob-nums a").forEach(function (a) {
    a.addEventListener("click", function (event) {
      event.preventDefault();
      var n = lineNumber(a.closest("tr"));
      if (n === null) {
        return;
      }
      if (event.shiftKey && anchorLine !== null) {
        selectRange(anchorLine, n);
        setHash(Math.min(anchorLine, n), Math.max(anchorLine, n));
      } else {
        anchorLine = n;
        selectRange(n, n);
        setHash(n, n);
      }
    });
  });

  function selectedRange() {
    var numbers = lineRows
      .filter(function (tr) {
        return tr.classList.contains("sel");
      })
      .map(lineNumber);
    if (numbers.length === 0) {
      return null;
    }
    return [Math.min.apply(null, numbers), Math.max.apply(null, numbers)];
  }

  function openComposer() {
    var range = selectedRange();
    var template = document.getElementById("composer-template");
    if (!range || !template) {
      return;
    }
    var existing = table.querySelector("tr.blob-composer");
    if (existing) {
      existing.remove();
    }

    // Land below the last selected line's own row, and below any comment
    // cards the server already interleaved after it.
    var afterRow = byNumber[range[1]];
    if (!afterRow) {
      return;
    }
    while (
      afterRow.nextElementSibling &&
      afterRow.nextElementSibling.classList.contains("blob-comment-row")
    ) {
      afterRow = afterRow.nextElementSibling;
    }

    var tr = document.createElement("tr");
    tr.className = "blob-composer";
    var td = document.createElement("td");
    td.colSpan = 2;

    var fragment = template.content.cloneNode(true);
    var form = fragment.querySelector("form");
    var linesInput = form && form.querySelector('input[name="lines"]');
    if (linesInput) {
      linesInput.value =
        range[0] === range[1] ? String(range[0]) : range[0] + ":" + range[1];
    }
    var cancel = fragment.querySelector(".composer-cancel");
    if (cancel) {
      cancel.addEventListener("click", function () {
        tr.remove();
      });
    }

    td.appendChild(fragment);
    tr.appendChild(td);
    afterRow.parentNode.insertBefore(tr, afterRow.nextElementSibling);
  }

  // One "+" affordance per line row, injected once -- CSS reveals it on
  // row hover (`.blob tr:hover .blob-add`), so there is nothing to
  // rebuild on each click.
  lineRows.forEach(function (tr) {
    var cell = tr.querySelector("td.blob-nums");
    if (!cell) {
      return;
    }
    var button = document.createElement("button");
    button.type = "button";
    button.className = "blob-add";
    button.setAttribute("aria-label", "Comment on this line");
    button.textContent = "+";
    button.addEventListener("click", function (event) {
      event.preventDefault();
      var n = lineNumber(tr);
      if (n === null) {
        return;
      }
      if (anchorLine === null || !tr.classList.contains("sel")) {
        anchorLine = n;
        selectRange(n, n);
        setHash(n, n);
      }
      openComposer();
    });
    cell.appendChild(button);
  });
})();

/*
 * Standalone comment triggers -- "comment on this file"
 * (`crate::pages::files::blob_header`) and "comment on this commit"
 * (`crate::pages::commits::commit_comment_template`) -- toggle a
 * server-rendered `<template>` as a floating popup right under their own
 * header/meta row, instead of navigating to a full add-comment page.
 * Each trigger's `href` stays a real no-JS fallback.
 */
(function () {
  "use strict";

  document.querySelectorAll("a.composer-trigger[data-composer]").forEach(function (trigger) {
    var template = document.getElementById(trigger.getAttribute("data-composer"));
    var host = trigger.closest(".blob-header, .commit-meta");
    if (!template || !host) {
      return;
    }
    trigger.addEventListener("click", function (event) {
      event.preventDefault();
      var existing = host.querySelector(".standalone-composer");
      if (existing) {
        existing.remove();
        return;
      }
      var wrapper = document.createElement("div");
      wrapper.className = "standalone-composer";
      wrapper.appendChild(template.content.cloneNode(true));
      var cancel = wrapper.querySelector(".composer-cancel");
      if (cancel) {
        cancel.addEventListener("click", function () {
          wrapper.remove();
        });
      }
      host.appendChild(wrapper);
      var textarea = wrapper.querySelector("textarea");
      if (textarea) {
        textarea.focus();
      }
    });
  });
})();
