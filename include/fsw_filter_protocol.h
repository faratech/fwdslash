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

typedef struct _FSW_MAPPING_MESSAGE {
  ULONG Version;
  ULONG Size;
  ULONG Operation;
  ULONG Reserved;
  ULONGLONG Generation;
  ULONG DistributionCount;
  WCHAR Distributions[FSW_MAX_DISTRIBUTIONS][FSW_MAX_DISTRIBUTION_NAME];
} FSW_MAPPING_MESSAGE, *PFSW_MAPPING_MESSAGE;
