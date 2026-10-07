import type { CodeViewLineSelection } from "@pierre/diffs";

type SelectionContext = { item: { id: string } };

export function commentSelectionCallbacks(
  openComposer: (selection: CodeViewLineSelection) => void,
) {
  return {
    onLineSelectionEnd(
      range: CodeViewLineSelection["range"] | null,
      context: SelectionContext,
    ) {
      if (!range) return;
      openComposer({ id: context.item.id, range });
    },
  };
}
