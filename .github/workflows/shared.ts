import {
  effectUtilsBinaryCaches,
  namespaceRunner,
  plainFlakeSetupSteps,
  RUNNER_PROFILES,
  type BinaryCacheDescriptor,
} from '../../repos/effect-utils/genie/external.ts'

const effectUtilsCache: BinaryCacheDescriptor =
  effectUtilsBinaryCaches['overeng-effect-utils'] ??
  (() => {
    throw new Error('effect-utils no longer publishes the overeng-effect-utils cache descriptor')
  })()

/** Linux jobs run on Namespace, pinned to their own workflow run. */
export const linuxRunner = namespaceRunner({
  profile: RUNNER_PROFILES[0],
  runId: '${{ github.run_id }}',
})

/** Release macOS jobs use the same run-scoped Namespace affinity. */
export const macosRunner = namespaceRunner({
  profile: RUNNER_PROFILES[1],
  runId: '${{ github.run_id }}',
})

/** Determinate Nix with effect-utils' public cache, read-only, so genie artifacts substitute. */
export const nixSetupOptions = { binaryCaches: [effectUtilsCache] }

export const nixSetupSteps = () => plainFlakeSetupSteps({ nix: nixSetupOptions })
