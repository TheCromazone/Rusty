# Isolated scroll-metric harness

This Xcode project wraps only the stage-0 spike. It does not create the stage-1
Rusty app, add app code to RustyKit, or alter the CLI.

Targets: a static `DiffPrototypeCore` framework, a native AppKit app using the
same spike sources, and `DiffPrototypeUITests`. The UI-test source requests
`XCTOSSignpostMetric.scrollingAndDecelerationMetric` with ten iterations for each
of 20k collapsed, 20k expanded and 100k expanded. Each iteration flings upward,
stops measurement, and flings downward outside the measured interval.

Run only in an agreed quiet GUI window, with no simultaneous Instruments or
other UI-test driver. No paid providers, network or cloud resource are used.

```sh
xcodebuild test \
  -project apps/macos/DiffPrototype/UIHarness/DiffPrototypeHarness.xcodeproj \
  -scheme DiffPrototypeUITests -configuration Release \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath apps/macos/DiffPrototype/.build/ui-dd \
  -resultBundlePath apps/macos/DiffPrototype/evidence/scroll-quiet-release.xcresult \
  -parallel-testing-enabled NO -jobs 2 \
  -test-timeouts-enabled YES \
  -default-test-execution-time-allowance 90 \
  -maximum-test-execution-time-allowance 120 \
  CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=-
```

The harness uses local ad hoc signatures. Disabling code signing leaves the copied
XCTest runner signature invalid after its bundle is assembled; macOS can reject
the runner before any test method starts. No developer account is required. Both the app and runner must pass
`codesign --verify --deep --strict` before a measurement attempt.

A later enumerate-only preflight reproduced the bootstrap failure: the runner
was killed by macOS security policy while its copied signature was invalid.
After enabling ad hoc signing, strict signature verification passed and
enumeration returned all three methods with an empty `errors` array. This
proves test discovery, not scrolling performance. Check the enumeration JSON:
`xcodebuild` can exit zero while that JSON contains a runner error.

Use `test-without-building` in place of `test` to use the already-compiled
harness. A fresh result path is required each time. Do not delete or overwrite
prior results. If the test runner never enters a method, retain the runner logs
and report zero test methods executed; a launched runner is not a passing test.
The first loaded-machine attempt was interrupted after compilation when no test
method had begun; its result reports a runner-level cancellation failure.

```sh
xcrun xcresulttool get test-results summary --path <result.xcresult>
xcrun xcresulttool get test-results metrics --path <result.xcresult>
```

Extract the actual reported metric names, units, samples and iteration counts.
If no hitch metric is emitted for AppKit scrolling, mark the metric unavailable;
do not convert paint times or UI-test duration to a hitch ratio. The required
bar is a valid observed hitch ratio below 5 ms/s on a quiet documented Mac.

An Animation Hitches trace is a separate diagnostic run. Keep it bounded by time
and disk, and drive actual input with CUA. The first 30-second attempt produced
4.7 GiB in the trace plus a 4.7 GiB raw temporary ktrace and did not finish
finalizing; both incomplete files were removed to protect disk space. Preserve
`evidence/animation-hitches-failed-attempt.json` and its log as a failure receipt.
No valid trace or hitch metric exists from that run. A future attempt should use
a much shorter capture and a small recording window, monitor both the output
trace and its raw temp, and reserve ample disk before starting. Do not retry
until the coordinator explicitly opens the quiet window.

Functional launch checks run separately from scrolling metrics. They verify
initial viewport bounds and compare the actual draw-loop projection with AX
children before and after global and individual folding. The latter keeps
window screenshots and JSON diagnostic attachments in its result bundle.
`--projection-diagnostics <path>` enables bounded last-draw/AX snapshots for
this test only; leave it off during performance measurements. Draw-loop
agreement does not alone prove what WindowServer presented: inspect the saved
screenshots as well. An intermittent initial-frame discrepancy remains
unexplained unless a controlled same-process reproduction establishes it.
