import { expect, test } from "bun:test";
import {
  commentIconMask,
  icon,
  seenIconMask,
  treeIcons,
} from "./icons";

test("review controls render Font Awesome icons", () => {
  const settings = icon("settings");

  expect(settings).toContain('data-prefix="fas"');
  expect(settings).toContain('data-icon="gear"');
  expect(settings).not.toContain("transform=");
});

test("file-tree icons and indicators come from Font Awesome", () => {
  expect(treeIcons.set).toBe("none");
  expect(treeIcons.spriteSheet).toContain('id="tact-fa-file"');
  expect(treeIcons.spriteSheet).toContain('id="tact-fa-chevron-right"');
  expect(treeIcons.remap["file-tree-icon-file"].name).toBe("tact-fa-file");
  expect(commentIconMask).toStartWith("data:image/svg+xml,");
  expect(decodeURIComponent(commentIconMask)).toContain('data-icon="comment"');
  expect(decodeURIComponent(seenIconMask)).toContain('data-icon="circle-check"');
});
