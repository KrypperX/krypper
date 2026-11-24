# Glossary

- **Declared capability:** A provider or model feature reported by trusted provider metadata or the offline model catalog. It describes what the source claims, not what the current account can use.
- **Observed evidence:** A bounded result from an actual provider request, such as health, latency, rate limiting, or token usage.
- **Verified capability:** A capability confirmed by an explicit probe for a specific provider, model, and configuration revision. Verification applies only to the behavior the probe exercised.
- **Discoverable provider:** A provider Krypper can identify from its provider metadata or a valid configured profile.
- **Executable provider:** A provider whose declared protocol has a registered runtime adapter.
- **Configured provider:** A provider with the required user settings and credentials available for the selected account.
- **Configuration revision:** An opaque identity for the provider, model, and effective configuration used to scope runtime evidence. A different account or credential has a different revision.
