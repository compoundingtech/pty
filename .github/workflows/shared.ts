import {
  defaultActionlintConfig as baseActionlintConfig,
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
  profile: `${RUNNER_PROFILES[0]};job.priority=2`,
  runId: '${{ github.run_id }}',
})

/** Release macOS jobs use the same run-scoped Namespace affinity. */
export const macosRunner = namespaceRunner({
  profile: `${RUNNER_PROFILES[1]};job.priority=2`,
  runId: '${{ github.run_id }}',
})

/** Determinate Nix with effect-utils' public cache, read-only, so genie artifacts substitute. */
export const nixSetupOptions = { binaryCaches: [effectUtilsCache] }

export const nixSetupSteps = () => plainFlakeSetupSteps({ nix: nixSetupOptions })

/** Superseded revisions share a group only within one PR and workflow. */
export const pullRequestConcurrency = {
  group: '${{ github.workflow }}-${{ github.event_name }}-${{ github.event.pull_request.number || github.run_id }}',
  'cancel-in-progress': "${{ github.event_name == 'pull_request' }}",
}

/** Keep actionlint strict while recognizing the two prioritized profiles. */
export const defaultActionlintConfig = {
  ...baseActionlintConfig,
  selfHostedRunnerLabels: [
    ...(baseActionlintConfig.selfHostedRunnerLabels ?? []),
    `${RUNNER_PROFILES[0]};job.priority=2`,
    `${RUNNER_PROFILES[1]};job.priority=2`,
  ],
}
