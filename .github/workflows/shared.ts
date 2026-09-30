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

/** Pull-request jobs run on Namespace Linux, pinned to their own workflow run. */
export const linuxRunner = namespaceRunner({
  profile: RUNNER_PROFILES[0],
  runId: '${{ github.run_id }}',
})

/** Determinate Nix with effect-utils' public cache, read-only, so genie artifacts substitute. */
export const nixSetupOptions = { binaryCaches: [effectUtilsCache] }

export const nixSetupSteps = () => plainFlakeSetupSteps({ nix: nixSetupOptions })
