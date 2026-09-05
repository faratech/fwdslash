#pragma once

#define FSW_FILTER_PORT_NAME L"\\FswFilterPort"
#define FSW_PROTOCOL_VERSION 2u
#define FSW_MAX_DISTRIBUTIONS 32u
#define FSW_MAX_DISTRIBUTION_NAME 128u

typedef enum _FSW_MESSAGE_OPERATION {
  FswOperationReplaceMappings = 1,
  FswOperationClearMappings = 2,
  FswOperationPing = 3,
} FSW_MESSAGE_OPERATION;

/*
 * Ping reply contract.
 *
 * FswOperationPing always succeeds. When the caller supplies an output buffer
 * of at least sizeof(ULONG) the driver writes FSW_PROTOCOL_VERSION into it and
 * returns sizeof(ULONG) as the returned output length, so a client can report
 * the protocol the *loaded* driver speaks rather than the one it was compiled
 * against. A caller that passes no output buffer, or a shorter one, gets the
 * original behaviour: success and a returned length of zero. Every other
 * operation returns no output.
 *
 * The client must send the whole FSW_MAPPING_MESSAGE for a ping as well; the
 * driver rejects any input length other than sizeof(FSW_MAPPING_MESSAGE).
 */

typedef struct _FSW_MAPPING_MESSAGE {
  ULONG Version;
  ULONG Size;
  ULONG Operation;
  ULONG Reserved;
  ULONGLONG Generation;
  ULONG DistributionCount;
  WCHAR Distributions[FSW_MAX_DISTRIBUTIONS][FSW_MAX_DISTRIBUTION_NAME];
} FSW_MAPPING_MESSAGE, *PFSW_MAPPING_MESSAGE;
