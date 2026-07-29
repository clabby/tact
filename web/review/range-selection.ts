export type ReviewScope = "uncommitted" | "full_branch";

export type RangeOption = {
  scope: ReviewScope;
  label: string;
  description: string;
  from: string;
  to: string;
  recommended: boolean;
};

export function rangeOptions(trunk: string): RangeOption[] {
  return [
    {
      scope: "uncommitted",
      label: "Uncommitted changes",
      description: "Review only the edits currently in your working tree.",
      from: "HEAD",
      to: "Working tree",
      recommended: true,
    },
    {
      scope: "full_branch",
      label: "Full branch",
      description: "Review every committed and uncommitted change on this branch.",
      from: trunk,
      to: "Working tree",
      recommended: false,
    },
  ];
}

export function rangeOption(scope: ReviewScope, trunk: string) {
  return rangeOptions(trunk).find((option) => option.scope === scope) ?? rangeOptions(trunk)[0];
}
