# SMS integration: findings and implementation plan

For the Linux-generic product direction, see [Host service integration](architecture.md#host-service-integration-direction). That proposal extends this initial device investigation with a shared host message store, native-client access and Android history reconciliation. Moto details below are evidence and backend constraints, not requirements for Droidloom core. RCS does not work yet.

Status: investigation complete at the source/interface level; implementation and
live SMS send/receive validation pending. Reviewed 2026-09-12 on Moto Edge 70.
The user requested findings and a plan before implementation. No PIN was
submitted, no message was sent, and no SMS contents were read for this review.

## 1. How sending and receiving work

Keep ModemManager as the sole host owner of modem control and QMI WMS messaging.
The existing Qualcomm firmware exposes Wireless Messaging Service (QMI service
5). Its presence establishes a protocol endpoint, not working carrier SMS.
The installed adapted MM is based on 1.24.2 and includes the QMI messaging code.

For ordinary Linux SMS, the public D-Bus flow is:

- Send: `Modem.Messaging.Create` with destination and text, then `Sms.Send`.
  Creating an object does not send it. Observe completion/errors and SMS state.
  A successful send is not proof that the recipient received it. Request a
  delivery report where supported and correlate the returned status separately.
- Receive: subscribe to object/property changes and `Messaging.Added`, reconcile
  the initial message list, and wait for `Sms.State = RECEIVED`. An Added signal
  can represent only the first part of an incomplete multipart SMS.
- Reconnect: rebuild subscriptions after MM restarts and reconcile stored
  messages. MM object paths are not durable message identifiers.
- Text and binary payloads are exposed as `Text` and `Data`; `Data` is payload,
  not a generic raw-PDU transport. MM handles encoding and multipart assembly.

The MM Messaging interface requires an unlocked SIM. The last verified phone
state is `locked` / `sim-pin`, with 3 PIN1 and 10 PUK1 attempts remaining.
The instruction not to submit the PIN remains in force. Consequently neither
registration nor real SMS transport has been validated with this inserted SIM.

The network route remains a gating unknown. Existing host configuration disables
IMS registration because earlier IMS attempts caused a registration/detach loop.
Ordinary SMS need not use an application internet connection, but working LTE
data does not establish working SMS. Determine the actual available non-IMS SMS
route on Iliad before assuming SMS requires, or works without, an IMS repair.
Do not change IMS configuration merely to make Android report service.

### Initial live validation, after separate authorization

1. User unlocks the SIM or explicitly authorizes a single guarded unlock; check
   registration and advertised messaging support. Do not guess credentials.
2. User sends one known test SMS to the phone from another handset. Observe
   arrival, completion and storage behavior without deleting pre-existing SMS.
3. Obtain the exact destination and test text before any outbound message.
   Create/send one test and confirm receipt on the other handset. Record
   submission and delivery as separate results.
4. Test Unicode and concatenated messages, modem/MM restart, Android downtime,
   and receipt during deep suspend. Record latency and power impact. Prove
   whether incoming SMS wakes the host; do not assume existing RTC wake does.
5. If registration succeeds but SMS fails, investigate WMS errors, network
   service availability and IMS routing before building an Android workaround.

## 2. Droidloom and the Android Messages app

### Observed installed state

Read-only Android checks report Android 17 / API 37:

| Item | Observation |
| --- | --- |
| AOSP Messages | `com.android.messaging`, `/product/app/messaging/messaging.apk` |
| Default SMS role | Already held by `com.android.messaging` |
| Google Messages | `com.google.android.apps.messaging` not found |
| Framework packages | Phone, TelephonyProvider, CarrierConfig and MMS service installed |
| Phone process | `com.android.phone` running |
| Binder services | `phone`, `isms` and `isub` absent |
| Declared telephony features | None returned by `pm list features` |
| Vendor radio declaration | No radio declaration found in inspected VINTF/init paths |
| Droidloom code | Existing short proposal only; no SMS/radio host integration found |

Use the installed AOSP Messages app as the first acceptance target. Google
Messages would be a subsequent app compatibility test; this plan does not
promise RCS. Installing another SMS app alone cannot create the missing radio
and subscription services. A running Phone process is also insufficient.

### Proposed architecture

```text
AOSP Messages (default SMS role)
        ↕ Android SmsManager / telephony / TelephonyProvider
Droidloom Android radio HAL adapter (AIDL, cell-local Binder)
        ↕ private authenticated, versioned transport
Droidloom host radio broker (Rust, durable receipt/outbox state)
        ↕ ModemManager D-Bus + narrowly scoped packet-preserving extension
ModemManager QMI WMS → Qualcomm modem → carrier SMS service
```

Keep Android app permissions, the SMS role and the normal inbox/sent-message
provider. The HAL adapter handles radio events; do not directly insert inbox
rows or spoof SMS broadcasts as the integration. Keep host D-Bus and modem
device access outside the Android cell. Bind the broker to one explicitly
assigned cell and physical SIM; SIM replacement must not deliver the previous
SIM's queued messages to a different user or subscription.

### Critical interface mismatch

Android `IRadioMessaging.sendSms` accepts a `GsmSmsMessage`: SMSC plus encoded
SMS-SUBMIT TPDU. Incoming `newSms` and `newSmsStatusReport` carry encoded PDUs.
The public MM API exposes decoded/assembled messages and only the last part's
message reference; it does not provide an equivalent raw packet API.

A text-only conversion loses packet headers, original segmentation, port
addressing and potentially delivery-report correlation. Do not label that a
complete Android radio implementation. The recommended first design spike is
a small, versioned MM extension preserving outbound TPDUs, inbound PDUs,
per-part references and errors while retaining MM's ownership of WMS. Audit
existing MM packet parsing/sending before choosing the exact interface.
This is new work, not a capability of stock MM D-Bus.

Acknowledgements require particular care. The checked MM QMI code sends a
positive ACK for transfer-route messages before handing the raw part to its
message assembly, and these messages can have no modem storage. Android also
requires acknowledgement between incoming SMS/status indications. A broker
which merely subscribes to decoded MM messages cannot guarantee durability
across a crash in that gap. Design receipt persistence at the MM packet boundary
before positive network ACK, with bounded failure handling; Android ACK then
advances the durable delivery queue. Audit stored-message and transfer-route
paths separately. Do not wait indefinitely for a stopped Android cell to ACK
the carrier, and do not claim exactly-once delivery without crash/replay tests.

### Implementation sequence and acceptance gates

1. **Prove host SMS first.** Complete the live checks above. Output: known
   carrier route, errors and storage behavior, successful inbound/outbound
   evidence. Keep the SIM locked until the user's restriction changes.
2. **Specify and test the packet boundary.** Fixture tests for GSM-7 extension
   characters, Unicode, concatenation headers, binary/port-addressed SMS,
   status reports and malformed input. Preserve Android TP-MR and per-part
   correlation. Test crashes before/after network ACK and Android ACK. Never
   automatically resend an outgoing request whose submission outcome is unknown.
3. **Implement host broker and MM extension.** New Rust broker under `runtime/`
   with separate protocol contracts; maintain the board MM changes alongside
   `tools/modem/patches` in the device workspace. Use peer credentials,
   supervisor-controlled socket access and explicit cell/SIM ownership. Persist
   pending inbound packets and outbound submission outcomes without logging
   message contents or secrets. Handle MM name-owner changes and modem removal.
   Use event-driven processing; no periodic modem polling or permanent wake lock.
4. **Bring up Android radio foundations.** Add an Android AIDL radio service,
   `Android.bp` build entries, init/VINTF declarations, capability XML and
   supervisor socket plumbing. Pin interfaces to the product's source lock.
   Implement required radio configuration, modem, SIM/UICC and network state
   plus messaging; enumerate actual startup requests before finalizing the
   minimum set. SMS alone is not enough to create a usable subscription. Return
   explicit unsupported results for out-of-scope operations; never invent
   registration, SIM readiness or voice/IMS capabilities. Confirm `phone`,
   `isms`, `isub`, subscription state and correct locked/absent-SIM behavior.
5. **Connect messaging.** Implement send/send-expect-more responses, incoming
   SMS and status indications, acknowledgement ordering, SMSC queries and
   storage-status behavior needed by the framework. Android owns app-facing
   permissions, conversation storage and multipart semantics. The host must not
   re-segment already encoded Android parts or duplicate retries.
6. **Validate through Messages.** Actual inbound conversation and notification,
   reply received by the other handset, Unicode/multipart, delivery state where
   supported, no duplicate replay across bridge/MM/Android restart, and pending
   receipt while Android is stopped. Test no SIM, locked SIM, SIM insertion,
   removal, network loss, reboot and deep suspend. Measure standby impact against
   the battery baseline. Existing SIM-less-boot hot-insertion recovery is a
   known unresolved case and belongs in this acceptance suite.
7. **Package persistently.** Ship broker, Android radio service, protocol and
   required MM extension with compatible version checks and rollback artifacts.
   Activate Android image changes with a Droidloom restart, then validate a
   full boot as a separate persistence test. Preserve existing user/app data.

Initial scope: one physical SIM and SMS. Calls, call audio, MMS, RCS, full IMS,
multi-SIM and general cell-broadcast support are separate work. If carrier SMS
requires IMS here, revisit that dependency before committing to this scope.
No kernel change is justified by the present findings.

## Evidence and references

- Device startup and current limitations:
  `/mnt/development/moto70edge/tools/modem/installation.md`.
- Checked MM source: device workspace
  `build/modem-bringup-20260908/src/ModemManager/introspection/` and
  `src/mm-broadband-modem-qmi.c`, especially
  `messaging_event_report_indication_cb`.
- Checked Android source: Droidloom
  `.work/arch/build/aosp-source/hardware/interfaces/radio/aidl/android/hardware/radio/messaging/`
  (`GsmSmsMessage`, `IRadioMessaging`, `IRadioMessagingIndication`).
- [Upstream Android messaging HAL](https://android.googlesource.com/platform/hardware/interfaces/+/13c4f08ce85c62cfe65e1ad0936e8256cad3a22e/radio/aidl/android/hardware/radio/messaging/IRadioMessaging.aidl).
- Product source pins: `android/manifest/source-lock.json` and
  `android/manifest/m2-sparse-source-lock.json`.

No implementation, build, deployment, restart, SIM unlock or SMS transmission
was performed for this investigation. Runtime observations do not establish
that all checked source files exactly match the installed images.
