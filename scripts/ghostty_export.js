ObjC.import("AppKit");

const MAX_PRESERVED_CLIPBOARD_BYTES = 4 * 1024 * 1024;
const MAX_PRESERVED_CLIPBOARD_ITEMS = 64;
const MAX_PRESERVED_CLIPBOARD_TYPES = 256;
const EXPORT_ACTION = "write_screen_file:copy,vt";

function reply(ok, error, path) {
  const value = { ok: ok };
  if (error !== null) value.error = error;
  if (path !== null) value.path = path;
  return JSON.stringify(value);
}

function count(value) {
  return Number(value.count);
}

function preserveClipboard(pasteboard) {
  const initialChangeCount = Number(pasteboard.changeCount);
  const sourceItems = pasteboard.pasteboardItems;
  const copies = [];
  let totalBytes = 0;

  if (sourceItems !== undefined && sourceItems !== null) {
    if (count(sourceItems) > MAX_PRESERVED_CLIPBOARD_ITEMS) {
      throw new Error("clipboard_too_large");
    }
    for (let itemIndex = 0; itemIndex < count(sourceItems); itemIndex += 1) {
      const source = sourceItems.objectAtIndex(itemIndex);
      const destination = $.NSPasteboardItem.alloc.init;
      const types = source.types;
      if (count(types) > MAX_PRESERVED_CLIPBOARD_TYPES) {
        throw new Error("clipboard_too_large");
      }
      for (let typeIndex = 0; typeIndex < count(types); typeIndex += 1) {
        const type = types.objectAtIndex(typeIndex);
        const data = source.dataForType(type);
        if (data === undefined) throw new Error("clipboard_unavailable");
        totalBytes += Number(data.length);
        if (totalBytes > MAX_PRESERVED_CLIPBOARD_BYTES) {
          throw new Error("clipboard_too_large");
        }
        if (!destination.setDataForType(data, type)) {
          throw new Error("clipboard_unavailable");
        }
      }
      copies.push(destination);
    }
  }

  if (Number(pasteboard.changeCount) !== initialChangeCount) {
    throw new Error("clipboard_interference");
  }
  return { changeCount: initialChangeCount, items: copies };
}

function restoreClipboard(pasteboard, preserved, ourChangeCount) {
  if (Number(pasteboard.changeCount) !== ourChangeCount) {
    return "clipboard_interference";
  }
  pasteboard.clearContents;
  if (preserved.items.length > 0 && !pasteboard.writeObjects($(preserved.items))) {
    return "clipboard_restore_failed";
  }
  return null;
}

function validExportPath(path, temporaryRoots) {
  if (path.indexOf("\0") !== -1) return false;
  for (const temporaryRoot of temporaryRoots) {
    const root = temporaryRoot.replace(/\/+$/, "");
    if (!path.startsWith(root + "/")) continue;
    const pieces = path.slice(root.length + 1).split("/");
    if (pieces.length === 2 &&
        /^[A-Za-z0-9_-]{22}$/.test(pieces[0]) &&
        pieces[1] === "screen.txt") return true;
  }
  return false;
}

function run(argv) {
  if (argv.length !== 3) return reply(false, "invalid_arguments", null);
  const terminalID = String(argv[0]);
  const temporaryRoots = [String(argv[1]), String(argv[2])];
  const pasteboard = $.NSPasteboard.generalPasteboard;
  let preserved;
  try {
    preserved = preserveClipboard(pasteboard);
  } catch (error) {
    const code = String(error.message || error);
    return reply(false, code, null);
  }

  const ghostty = Application("Ghostty");
  const terminals = ghostty.terminals();
  let terminal = null;
  for (let index = 0; index < terminals.length; index += 1) {
    if (String(terminals[index].id()) === terminalID) {
      terminal = terminals[index];
      break;
    }
  }
  if (terminal === null) return reply(false, "terminal_not_found", null);

  let performed = false;
  try {
    performed = Boolean(ghostty.performAction(EXPORT_ACTION, { on: terminal }));
  } catch (_) {
    performed = false;
  }

  const exportedChangeCount = Number(pasteboard.changeCount);
  if (!performed) {
    return reply(false, "export_failed", null);
  }
  if (exportedChangeCount !== preserved.changeCount + 1) {
    return reply(false, "clipboard_interference", null);
  }

  let exportPath = null;
  const value = pasteboard.stringForType($.NSPasteboardTypeString);
  if (value !== undefined && value !== null) {
    exportPath = String(ObjC.unwrap(value));
  }
  if (exportPath === null || !validExportPath(exportPath, temporaryRoots)) {
    return reply(false, "invalid_export_path", null);
  }

  if (Number(pasteboard.changeCount) !== exportedChangeCount) {
    return reply(false, "clipboard_interference", null);
  }
  const restoreError = restoreClipboard(pasteboard, preserved, exportedChangeCount);
  if (restoreError !== null) return reply(false, restoreError, exportPath);
  return reply(true, null, exportPath);
}
