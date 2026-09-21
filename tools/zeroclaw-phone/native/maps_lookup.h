#ifndef MAPS_LOOKUP_H
#define MAPS_LOOKUP_H
#include <stddef.h>
#include <stdint.h>
// One synchronous lookup on the process main thread; run only in a disposable
// same-binary --maps-lookup or --maps-lookup-phone subprocess. Caller owns returned bytes via free.
// No user location, Contacts, calendar, or Maps-app UI is accessed.
int maps_lookup_json(const uint8_t *name, size_t name_len,
                     const uint8_t *locality, size_t locality_len,
                     uint32_t timeout_ms, uint8_t **output, size_t *output_len);
// The phone-only path requires strict E.164 and sends that number unchanged as
// the query. A returned listing still requires a structured phone comparison;
// MapKit does not guarantee exhaustive reverse-phone coverage.
int maps_lookup_phone_json(const uint8_t *phone, size_t phone_len,
                           uint32_t timeout_ms, uint8_t **output, size_t *output_len);
void maps_lookup_free(uint8_t *output);
#endif
