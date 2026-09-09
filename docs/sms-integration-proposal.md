# SMS integration proposal

Status: proposed; no Droidloom SMS integration is implemented.

Choose one modem owner. For a Linux-owned modem, adapt Android's radio/telephony
boundary to the host modem service (for example ModemManager), preserving normal
Android SMS APIs and permissions rather than adding an app-specific messaging API.

First scope: one SIM, subscription/registration state, send results, incoming SMS
and delivery reports. Validate modem/carrier support, multipart messages,
reconnects and duplicate prevention. Calls, MMS, RCS and full IMS support require
separate scope. Android-owned hardware instead needs a compatible Android radio
stack; exposing a device node alone is insufficient.
