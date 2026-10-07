import type { ModelCatalog, ModelInfo, Speed } from "./wire";

/** A tier a model really offers, and the preference that selects it. */
export type SpeedChoice = { preference: Speed; tier: Speed };

/**
 * The speeds worth offering for a model: each distinct tier it runs, reached through the first
 * preference that selects it. Preferences a model would only downgrade are not offered.
 */
export function speedChoices(model: ModelInfo | undefined, catalog: ModelCatalog): SpeedChoice[] {
  const choices: SpeedChoice[] = [];
  catalog.speeds.forEach((preference, index) => {
    const tier = model?.effective_speeds[index] ?? preference;
    if (!choices.some((choice) => choice.tier === tier)) choices.push({ preference, tier });
  });
  return choices;
}

/** The tier a saved preference runs at on the given model. */
export function effectiveSpeed(model: ModelInfo | undefined, catalog: ModelCatalog, preference: Speed): Speed {
  const index = catalog.speeds.indexOf(preference);
  return (index < 0 ? undefined : model?.effective_speeds[index]) ?? preference;
}
