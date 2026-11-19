---
title: Telemetry
description: "What data Zed collects and how to control telemetry settings."
---

# Telemetry in Zed

Zed collects anonymous telemetry to understand usage patterns and diagnose issues.

Telemetry falls into two categories:

- **Client-side**: Usage metrics and crash reports. You can disable these in settings.
- **Server-side**: Collected when using hosted services like AI or Collaboration. Required for these features to function.

## Configuring Telemetry Settings

You have full control over what data is sent out by Zed.
To enable or disable some or all telemetry types, open Settings ({#kb zed::OpenSettings}) and search for "telemetry", or add the following to your settings file:

```json [settings]
"telemetry": {
    "diagnostics": false,
    "metrics": false
},
```

## Dataflow

Telemetry is sent from the application to our servers every 5 minutes (or when 50 events accumulate), then routed to the appropriate service. We currently use:

- [Sentry](https://sentry.io): Crash-monitoring service - stores diagnostic events
- [Snowflake](https://snowflake.com): Data warehouse - stores both diagnostic and metric events
- [Hex](https://www.hex.tech): Dashboards and data exploration - accesses data stored in Snowflake
- [Amplitude](https://www.amplitude.com): Dashboards and data exploration - accesses data stored in Snowflake

## Types of Telemetry

### Diagnostics

Crash reports consist of a [minidump](https://learn.microsoft.com/en-us/windows/win32/debug/minidump-files) and debug metadata. Reports are sent on the next launch after a crash, allowing Zed to identify and fix issues without requiring you to file a bug report.

You can inspect what data is sent in the `CrashInfo` struct in [crates/crashes/src/crashes.rs](https://github.com/zed-industries/zed/blob/main/crates/crashes/src/crashes.rs). See also: [Debugging Crashes](./development/debugging-crashes.md).

### Client-Side Metrics

Client-side telemetry includes:

- File extensions of opened files
- Features and tools used within the editor
- Project statistics (e.g., number of files)
- Frameworks detected in your projects

This data does not include your code or sensitive project details. Events are sent over HTTPS and rate-limited.

Usage data is tied to a random telemetry ID. If you've authenticated, this ID may be linked to your email so Zed can analyze patterns over time and reach out for feedback.

To audit what Zed has reported, run {#action zed::OpenTelemetryLog} from the command palette or click `Help > View Telemetry Log`.

For the full list of event types, see the `Event` enum in [telemetry_events.rs](https://github.com/zed-industries/zed/blob/main/crates/telemetry_events/src/telemetry_events.rs).

### Server-Side Metrics

When using Zed's hosted services, we collect metadata for rate limiting and billing (e.g., token usage). Zed does not store your prompts or code unless you explicitly share feedback or opt into Edit Prediction training data collection.

For details on AI request paths and opt-in data sharing, see [AI Privacy](./ai/privacy-and-security.md) and [Feedback and Training Data](./ai/ai-improvement.md).

## Zed Business

Administrators on Zed Business can enforce a no-sharing policy org-wide; members can't opt into [Edit Prediction training data sharing](./ai/ai-improvement.md#edit-predictions) or [AI feedback ratings](./ai/ai-improvement.md#ai-feedback-with-ratings). See [Data Sharing](./business/admin-controls.md#data-sharing) in Admin Controls.

<!-- TODO: link to telemetry org-wide disable control once it ships (currently planned for a future release) -->

## Concerns and Questions

If you have concerns about telemetry, you can [open an issue](https://github.com/zed-industries/zed/issues/new/choose) or email hi@zed.dev.

## Lathe Update Checks {#lathe-update-checks}

Lathe release builds check for updates through a Cloudflare Worker. Each check
includes your Lathe version, release channel, operating system, and processor
architecture. When usage metrics are enabled and an installation ID is available,
it also includes a SHA-256 hash of that ID with a fixed Lathe-specific salt. The
raw installation ID is never sent by the update check. The hash stays the same
across checks so daily and weekly active installs can be counted.

The Worker records the hash, version, channel, operating system, architecture,
and country reported by Cloudflare. It does not store or log your IP address.
Cloudflare processes your IP address to serve the request and determine country.
Update activity measures installations that check for updates, rather than people
or time spent editing. Local builds using the default GitHub endpoint send no
Lathe telemetry headers.

To stop sending the install hash, open the Settings Editor, search for
"telemetry", and turn off metrics. Or add this to your settings.json:

```json [settings]
{
  "telemetry": {
    "metrics": false
  }
}
```

Update checks continue with version, channel, operating system, and architecture,
but the Worker records no usage point without an install hash. Asset downloads
continue directly from GitHub. These update pings are separate from the inherited
Zed telemetry described above.

The default extension registry is fetched through the same Worker. When metrics
are on, that request carries the same install hash, so the Worker can count how
many installs load the registry. It records the hash, the country, and that the
request was a registry fetch. Turning off metrics removes the hash from registry
fetches too. Registries you add yourself on other hosts never receive it.

## Lathe Usage Counts {#lathe-usage-counts}

Starting with Lathe 1.22, Lathe sends a small set of anonymous usage counts to
PostHog so the maintainer can see which features get used. Only these events are
sent: app opened, agent thread started, agent message sent, agent turn completed,
agent tool call authorized, and agent error shown.

Each event carries only the event name, the Lathe version, operating system,
processor architecture, whether it's a development or release build, and an
identifier made from a SHA-256 hash of your installation ID. The properties are
rebuilt from scratch rather than forwarded from Lathe's internal events, so
prompts, messages, file names, paths and code are never included. PostHog person
profiles and GeoIP lookup are turned off for these events.

Turning off metrics (`"telemetry": { "metrics": false }`, shown above) stops
these events too, including any that were queued before you turned it off.
