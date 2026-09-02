#include "fswfilter.h"

#define FSW_MAX_INTERACTIVE_SESSIONS 16u

typedef struct _FSW_CONNECTION_CONTEXT {
  PFLT_PORT ClientPort;
  ULONG SessionId;
  ULONG SidLength;
  UCHAR Sid[SECURITY_MAX_SID_SIZE];
} FSW_CONNECTION_CONTEXT, *PFSW_CONNECTION_CONTEXT;

typedef struct _FSW_SESSION_MAPPINGS {
  PFSW_CONNECTION_CONTEXT Owner;
  ULONG SessionId;
  ULONG SidLength;
  UCHAR Sid[SECURITY_MAX_SID_SIZE];
  ULONGLONG Generation;
  ULONG DistributionCount;
  WCHAR Distributions[FSW_MAX_DISTRIBUTIONS][FSW_MAX_DISTRIBUTION_NAME];
} FSW_SESSION_MAPPINGS, *PFSW_SESSION_MAPPINGS;

typedef struct _FSW_GLOBALS {
  PFLT_FILTER Filter;
  PFLT_PORT ServerPort;
  EX_PUSH_LOCK MappingsLock;
  FSW_SESSION_MAPPINGS Mappings[FSW_MAX_INTERACTIVE_SESSIONS];
} FSW_GLOBALS;

FSW_GLOBALS Globals;

static NTSTATUS
FswGetProcessIdentity(_In_ PEPROCESS Process,
                      _Out_ PULONG SessionId,
                      _Out_writes_bytes_(SECURITY_MAX_SID_SIZE) PSID Sid,
                      _Out_ PULONG SidLength) {
  PACCESS_TOKEN token;
  PVOID sessionInformation = NULL;
  PVOID userInformation = NULL;
  PTOKEN_USER tokenUser;
  ULONG length;
  NTSTATUS status;

  token = PsReferencePrimaryToken(Process);
  status = SeQueryInformationToken(token, TokenSessionId, &sessionInformation);
  if (NT_SUCCESS(status)) {
    status = SeQueryInformationToken(token, TokenUser, &userInformation);
  }
  if (NT_SUCCESS(status)) {
    tokenUser = (PTOKEN_USER)userInformation;
    length = RtlLengthSid(tokenUser->User.Sid);
    if (length > SECURITY_MAX_SID_SIZE ||
        !RtlValidSid(tokenUser->User.Sid)) {
      status = STATUS_INVALID_SID;
    } else {
      *SessionId = *(PULONG)sessionInformation;
      *SidLength = length;
      status = RtlCopySid(SECURITY_MAX_SID_SIZE, Sid, tokenUser->User.Sid);
    }
  }
  if (userInformation != NULL) {
    ExFreePool(userInformation);
  }
  if (sessionInformation != NULL) {
    ExFreePool(sessionInformation);
  }
  PsDereferencePrimaryToken(token);
  return status;
}

static BOOLEAN
FswIsEligibleRequest(_In_ PFLT_CALLBACK_DATA Data) {
  PEPROCESS process;
  PACCESS_TOKEN token;
  PVOID integrityInformation = NULL;
  PVOID appContainerInformation = NULL;
  PVOID sessionInformation = NULL;
  PTOKEN_MANDATORY_LABEL label;
  ULONG integrityLevel = 0;
  ULONG sessionId = 0;
  BOOLEAN eligible = FALSE;
  NTSTATUS status;

  if (Data->RequestorMode != UserMode ||
      KeGetCurrentIrql() != PASSIVE_LEVEL) {
    return FALSE;
  }
  process = FltGetRequestorProcess(Data);
  if (process == NULL) {
    return FALSE;
  }
  token = PsReferencePrimaryToken(process);
  status = SeQueryInformationToken(token, TokenIntegrityLevel,
                                   &integrityInformation);
  if (NT_SUCCESS(status)) {
    label = (PTOKEN_MANDATORY_LABEL)integrityInformation;
    if (RtlValidSid(label->Label.Sid) &&
        *RtlSubAuthorityCountSid(label->Label.Sid) != 0) {
      integrityLevel = *RtlSubAuthoritySid(
          label->Label.Sid, *RtlSubAuthorityCountSid(label->Label.Sid) - 1);
    }
    status = SeQueryInformationToken(token, TokenIsAppContainer,
                                     &appContainerInformation);
  }
  if (NT_SUCCESS(status)) {
    status = SeQueryInformationToken(token, TokenSessionId,
                                     &sessionInformation);
  }
  if (NT_SUCCESS(status)) {
    sessionId = *(PULONG)sessionInformation;
    if (integrityLevel >= SECURITY_MANDATORY_MEDIUM_RID &&
        *(PULONG)appContainerInformation == 0 && sessionId != 0) {
      eligible = TRUE;
    }
  }
  if (sessionInformation != NULL) {
    ExFreePool(sessionInformation);
  }
  if (appContainerInformation != NULL) {
    ExFreePool(appContainerInformation);
  }
  if (integrityInformation != NULL) {
    ExFreePool(integrityInformation);
  }
  PsDereferencePrimaryToken(token);
  return eligible;
}

static BOOLEAN
FswIsValidDistributionName(
    _In_reads_(FSW_MAX_DISTRIBUTION_NAME) const WCHAR* Name) {
  ULONG length = 0;
  while (length < FSW_MAX_DISTRIBUTION_NAME &&
         Name[length] != UNICODE_NULL) {
    if (Name[length] == L'\\' || Name[length] == L'/' ||
        Name[length] == L':' || Name[length] < L' ') {
      return FALSE;
    }
    ++length;
  }
  if (length == 0 || length == FSW_MAX_DISTRIBUTION_NAME) {
    return FALSE;
  }
  if ((length == 1 && Name[0] == L'.') ||
      (length == 2 && Name[0] == L'.' && Name[1] == L'.')) {
    return FALSE;
  }
  return TRUE;
}

static VOID
FswClearMappingsForOwner(_In_ PFSW_CONNECTION_CONTEXT Owner) {
  KeEnterCriticalRegion();
  ExAcquirePushLockExclusive(&Globals.MappingsLock);
  for (ULONG index = 0; index < FSW_MAX_INTERACTIVE_SESSIONS; ++index) {
    if (Globals.Mappings[index].Owner == Owner) {
      RtlZeroMemory(&Globals.Mappings[index],
                    sizeof(Globals.Mappings[index]));
      break;
    }
  }
  ExReleasePushLockExclusive(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
}

static NTSTATUS
FswBuildPortSecurityDescriptor(_Outptr_ PSECURITY_DESCRIPTOR* Descriptor) {
  const ULONG aclSize = sizeof(ACL) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeLocalSystemSid)) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeAliasAdminsSid)) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeInteractiveSid));
  const ULONG totalSize = SECURITY_DESCRIPTOR_MIN_LENGTH + aclSize;
  PUCHAR memory = ExAllocatePool2(POOL_FLAG_PAGED, totalSize, FSW_POOL_TAG);
  PACL acl;
  NTSTATUS status;

  if (memory == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  RtlZeroMemory(memory, totalSize);
  *Descriptor = (PSECURITY_DESCRIPTOR)memory;
  acl = (PACL)(memory + SECURITY_DESCRIPTOR_MIN_LENGTH);
  status = RtlCreateSecurityDescriptor(*Descriptor,
                                       SECURITY_DESCRIPTOR_REVISION);
  if (NT_SUCCESS(status)) {
    status = RtlCreateAcl(acl, aclSize, ACL_REVISION);
  }
  if (NT_SUCCESS(status)) {
    status = RtlAddAccessAllowedAce(acl, ACL_REVISION, FLT_PORT_ALL_ACCESS,
                                    SeExports->SeLocalSystemSid);
  }
  if (NT_SUCCESS(status)) {
    status = RtlAddAccessAllowedAce(acl, ACL_REVISION, FLT_PORT_ALL_ACCESS,
                                    SeExports->SeAliasAdminsSid);
  }
  if (NT_SUCCESS(status)) {
    status = RtlAddAccessAllowedAce(acl, ACL_REVISION, FLT_PORT_ALL_ACCESS,
                                    SeExports->SeInteractiveSid);
  }
  if (NT_SUCCESS(status)) {
    status = RtlSetDaclSecurityDescriptor(*Descriptor, TRUE, acl, FALSE);
  }
  if (!NT_SUCCESS(status)) {
    ExFreePoolWithTag(memory, FSW_POOL_TAG);
    *Descriptor = NULL;
  }
  return status;
}

DRIVER_INITIALIZE DriverEntry;
NTSTATUS FswUnload(_In_ FLT_FILTER_UNLOAD_FLAGS Flags);
NTSTATUS FswInstanceSetup(_In_ PCFLT_RELATED_OBJECTS FltObjects,
                          _In_ FLT_INSTANCE_SETUP_FLAGS Flags,
                          _In_ DEVICE_TYPE VolumeDeviceType,
                          _In_ FLT_FILESYSTEM_TYPE VolumeFilesystemType);
FLT_PREOP_CALLBACK_STATUS
FswPreCreate(_Inout_ PFLT_CALLBACK_DATA Data,
             _In_ PCFLT_RELATED_OBJECTS FltObjects,
             _Flt_CompletionContext_Outptr_ PVOID* CompletionContext);
NTSTATUS FswPortConnect(
    _In_ PFLT_PORT ClientPort,
    _In_opt_ PVOID ServerPortCookie,
    _In_reads_bytes_opt_(SizeOfContext) PVOID ConnectionContext,
    _In_ ULONG SizeOfContext,
    _Outptr_result_maybenull_ PVOID* ConnectionPortCookie);
VOID FswPortDisconnect(_In_opt_ PVOID ConnectionCookie);
NTSTATUS FswPortMessage(
    _In_opt_ PVOID PortCookie,
    _In_reads_bytes_opt_(InputBufferLength) PVOID InputBuffer,
    _In_ ULONG InputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ReturnOutputBufferLength)
        PVOID OutputBuffer,
    _In_ ULONG OutputBufferLength,
    _Out_ PULONG ReturnOutputBufferLength);

CONST FLT_OPERATION_REGISTRATION Operations[] = {
    {IRP_MJ_CREATE, 0, FswPreCreate, NULL},
    {IRP_MJ_OPERATION_END}};

CONST FLT_REGISTRATION Registration = {
    sizeof(FLT_REGISTRATION), FLT_REGISTRATION_VERSION, 0, NULL, Operations,
    FswUnload, FswInstanceSetup, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
    NULL, NULL};

#ifdef ALLOC_PRAGMA
#pragma alloc_text(INIT, DriverEntry)
#pragma alloc_text(PAGE, FswUnload)
#pragma alloc_text(PAGE, FswInstanceSetup)
#pragma alloc_text(PAGE, FswPreCreate)
#pragma alloc_text(PAGE, FswPortConnect)
#pragma alloc_text(PAGE, FswPortDisconnect)
#pragma alloc_text(PAGE, FswPortMessage)
#endif

static BOOLEAN
FswFindDistribution(_In_ ULONG SessionId,
                    _In_ PSID Sid,
                    _In_ PUNICODE_STRING RelativeName,
                    _Out_ PUNICODE_STRING Distribution,
                    _Out_ PUNICODE_STRING Remainder) {
  USHORT characterIndex;
  UNICODE_STRING firstComponent;
  BOOLEAN found = FALSE;

  while (RelativeName->Length >= sizeof(WCHAR) &&
         RelativeName->Buffer[0] == L'\\') {
    RelativeName->Buffer += 1;
    RelativeName->Length -= sizeof(WCHAR);
    RelativeName->MaximumLength -= sizeof(WCHAR);
  }
  if (RelativeName->Length == 0) {
    return FALSE;
  }
  firstComponent = *RelativeName;
  for (characterIndex = 0;
       characterIndex < RelativeName->Length / sizeof(WCHAR);
       ++characterIndex) {
    if (RelativeName->Buffer[characterIndex] == L'\\') {
      firstComponent.Length = characterIndex * sizeof(WCHAR);
      firstComponent.MaximumLength = firstComponent.Length;
      break;
    }
  }

  KeEnterCriticalRegion();
  ExAcquirePushLockShared(&Globals.MappingsLock);
  for (ULONG slotIndex = 0; slotIndex < FSW_MAX_INTERACTIVE_SESSIONS;
       ++slotIndex) {
    PFSW_SESSION_MAPPINGS slot = &Globals.Mappings[slotIndex];
    if (slot->Owner == NULL || slot->SessionId != SessionId ||
        !RtlEqualSid((PSID)slot->Sid, Sid)) {
      continue;
    }
    for (ULONG index = 0; index < slot->DistributionCount; ++index) {
      UNICODE_STRING candidate;
      RtlInitUnicodeString(&candidate, slot->Distributions[index]);
      if (RtlEqualUnicodeString(&candidate, &firstComponent, TRUE)) {
        *Distribution = firstComponent;
        found = TRUE;
        break;
      }
    }
    break;
  }
  if (found) {
    const USHORT consumed = firstComponent.Length;
    Remainder->Buffer = RelativeName->Buffer + consumed / sizeof(WCHAR);
    Remainder->Length = RelativeName->Length - consumed;
    Remainder->MaximumLength = Remainder->Length;
  }
  ExReleasePushLockShared(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
  return found;
}

static NTSTATUS
FswBuildTargetName(_In_ PUNICODE_STRING Distribution,
                   _In_ PUNICODE_STRING Remainder,
                   _Out_ PUNICODE_STRING TargetName) {
  const UNICODE_STRING prefix =
      RTL_CONSTANT_STRING(L"\\??\\UNC\\wsl.localhost\\");
  const ULONG required = prefix.Length + Distribution->Length +
                         Remainder->Length + sizeof(WCHAR);
  if (required > MAXUSHORT) {
    return STATUS_NAME_TOO_LONG;
  }
  TargetName->Buffer = ExAllocatePool2(POOL_FLAG_PAGED, required, FSW_POOL_TAG);
  if (TargetName->Buffer == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  TargetName->Length = 0;
  TargetName->MaximumLength = (USHORT)required;
  RtlAppendUnicodeStringToString(TargetName, &prefix);
  RtlAppendUnicodeStringToString(TargetName, Distribution);
  RtlAppendUnicodeStringToString(TargetName, Remainder);
  TargetName->Buffer[TargetName->Length / sizeof(WCHAR)] = UNICODE_NULL;
  return STATUS_SUCCESS;
}

NTSTATUS
FswInstanceSetup(_In_ PCFLT_RELATED_OBJECTS FltObjects,
                 _In_ FLT_INSTANCE_SETUP_FLAGS Flags,
                 _In_ DEVICE_TYPE VolumeDeviceType,
                 _In_ FLT_FILESYSTEM_TYPE VolumeFilesystemType) {
  UNREFERENCED_PARAMETER(FltObjects);
  UNREFERENCED_PARAMETER(Flags);
  UNREFERENCED_PARAMETER(VolumeFilesystemType);
  PAGED_CODE();
  return VolumeDeviceType == FILE_DEVICE_DISK_FILE_SYSTEM
             ? STATUS_SUCCESS
             : STATUS_FLT_DO_NOT_ATTACH;
}

FLT_PREOP_CALLBACK_STATUS
FswPreCreate(_Inout_ PFLT_CALLBACK_DATA Data,
             _In_ PCFLT_RELATED_OBJECTS FltObjects,
             _Flt_CompletionContext_Outptr_ PVOID* CompletionContext) {
  PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
  UNICODE_STRING relativeName;
  UNICODE_STRING distribution;
  UNICODE_STRING remainder;
  UNICODE_STRING targetName = {0};
  UCHAR sid[SECURITY_MAX_SID_SIZE];
  ULONG sidLength;
  ULONG sessionId;
  PEPROCESS process;
  NTSTATUS status;

  UNREFERENCED_PARAMETER(CompletionContext);
  PAGED_CODE();
  if (!FLT_IS_IRP_OPERATION(Data) || !FswIsEligibleRequest(Data) ||
      FlagOn(Data->Iopb->OperationFlags, SL_OPEN_PAGING_FILE) ||
      FlagOn(Data->Iopb->TargetFileObject->Flags, FO_VOLUME_OPEN) ||
      FlagOn(Data->Iopb->Parameters.Create.Options, FILE_OPEN_BY_FILE_ID)) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  process = FltGetRequestorProcess(Data);
  if (process == NULL ||
      !NT_SUCCESS(FswGetProcessIdentity(process, &sessionId, (PSID)sid,
                                        &sidLength))) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  UNREFERENCED_PARAMETER(sidLength);
  status = FltGetFileNameInformation(Data,
                                     FLT_FILE_NAME_OPENED |
                                         FLT_FILE_NAME_QUERY_DEFAULT,
                                     &nameInfo);
  if (!NT_SUCCESS(status)) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  status = FltParseFileNameInformation(nameInfo);
  if (!NT_SUCCESS(status) || nameInfo->Volume.Length >= nameInfo->Name.Length) {
    FltReleaseFileNameInformation(nameInfo);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  relativeName.Buffer =
      nameInfo->Name.Buffer + nameInfo->Volume.Length / sizeof(WCHAR);
  relativeName.Length = nameInfo->Name.Length - nameInfo->Volume.Length;
  relativeName.MaximumLength = relativeName.Length;
  if (!FswFindDistribution(sessionId, (PSID)sid, &relativeName, &distribution,
                           &remainder)) {
    FltReleaseFileNameInformation(nameInfo);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  status = FswBuildTargetName(&distribution, &remainder, &targetName);
  if (NT_SUCCESS(status)) {
    status = IoReplaceFileObjectName(FltObjects->FileObject, targetName.Buffer,
                                     targetName.Length);
  }
  if (targetName.Buffer != NULL) {
    ExFreePoolWithTag(targetName.Buffer, FSW_POOL_TAG);
  }
  FltReleaseFileNameInformation(nameInfo);
  if (!NT_SUCCESS(status)) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  Data->IoStatus.Status = STATUS_REPARSE;
  Data->IoStatus.Information = IO_REPARSE;
  return FLT_PREOP_COMPLETE;
}

NTSTATUS
FswPortConnect(_In_ PFLT_PORT ClientPort,
               _In_opt_ PVOID ServerPortCookie,
               _In_reads_bytes_opt_(SizeOfContext) PVOID ConnectionContext,
               _In_ ULONG SizeOfContext,
               _Outptr_result_maybenull_ PVOID* ConnectionPortCookie) {
  PFSW_CONNECTION_CONTEXT context;
  NTSTATUS status;
  UNREFERENCED_PARAMETER(ServerPortCookie);
  UNREFERENCED_PARAMETER(ConnectionContext);
  UNREFERENCED_PARAMETER(SizeOfContext);
  PAGED_CODE();

  context = ExAllocatePool2(POOL_FLAG_PAGED, sizeof(*context), FSW_POOL_TAG);
  if (context == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  RtlZeroMemory(context, sizeof(*context));
  context->ClientPort = ClientPort;
  status = FswGetProcessIdentity(PsGetCurrentProcess(), &context->SessionId,
                                 (PSID)context->Sid, &context->SidLength);
  if (!NT_SUCCESS(status) || context->SessionId == 0) {
    ExFreePoolWithTag(context, FSW_POOL_TAG);
    return NT_SUCCESS(status) ? STATUS_ACCESS_DENIED : status;
  }
  *ConnectionPortCookie = context;
  return STATUS_SUCCESS;
}

VOID
FswPortDisconnect(_In_opt_ PVOID ConnectionCookie) {
  PFSW_CONNECTION_CONTEXT context =
      (PFSW_CONNECTION_CONTEXT)ConnectionCookie;
  PAGED_CODE();
  if (context == NULL) {
    return;
  }
  FswClearMappingsForOwner(context);
  FltCloseClientPort(Globals.Filter, &context->ClientPort);
  ExFreePoolWithTag(context, FSW_POOL_TAG);
}

NTSTATUS
FswPortMessage(_In_opt_ PVOID PortCookie,
               _In_reads_bytes_opt_(InputBufferLength) PVOID InputBuffer,
               _In_ ULONG InputBufferLength,
               _Out_writes_bytes_to_opt_(OutputBufferLength,
                                         *ReturnOutputBufferLength)
                   PVOID OutputBuffer,
               _In_ ULONG OutputBufferLength,
               _Out_ PULONG ReturnOutputBufferLength) {
  PFSW_CONNECTION_CONTEXT context = (PFSW_CONNECTION_CONTEXT)PortCookie;
  PFSW_MAPPING_MESSAGE message;
  PFSW_SESSION_MAPPINGS slot = NULL;
  NTSTATUS status = STATUS_SUCCESS;
  UNREFERENCED_PARAMETER(OutputBuffer);
  UNREFERENCED_PARAMETER(OutputBufferLength);
  PAGED_CODE();

  *ReturnOutputBufferLength = 0;
  if (context == NULL || InputBuffer == NULL ||
      InputBufferLength != sizeof(FSW_MAPPING_MESSAGE)) {
    return STATUS_INFO_LENGTH_MISMATCH;
  }
  message = (PFSW_MAPPING_MESSAGE)InputBuffer;
  if (message->Version != FSW_PROTOCOL_VERSION ||
      message->Size != sizeof(FSW_MAPPING_MESSAGE) || message->Reserved != 0 ||
      message->DistributionCount > FSW_MAX_DISTRIBUTIONS) {
    return STATUS_INVALID_PARAMETER;
  }
  if (message->Operation == FswOperationPing) {
    return STATUS_SUCCESS;
  }
  if (message->Operation != FswOperationReplaceMappings &&
      message->Operation != FswOperationClearMappings) {
    return STATUS_INVALID_PARAMETER;
  }
  for (ULONG index = 0; index < message->DistributionCount; ++index) {
    if (!FswIsValidDistributionName(message->Distributions[index])) {
      return STATUS_INVALID_PARAMETER;
    }
  }

  KeEnterCriticalRegion();
  ExAcquirePushLockExclusive(&Globals.MappingsLock);
  for (ULONG index = 0; index < FSW_MAX_INTERACTIVE_SESSIONS; ++index) {
    if (Globals.Mappings[index].Owner == context) {
      slot = &Globals.Mappings[index];
      break;
    }
    if (slot == NULL && Globals.Mappings[index].Owner == NULL) {
      slot = &Globals.Mappings[index];
    }
  }
  if (slot == NULL) {
    status = STATUS_INSUFFICIENT_RESOURCES;
  } else if (slot->Owner == context &&
             message->Generation < slot->Generation) {
    status = STATUS_REVISION_MISMATCH;
  } else if (message->Operation == FswOperationClearMappings) {
    RtlZeroMemory(slot, sizeof(*slot));
  } else {
    RtlZeroMemory(slot, sizeof(*slot));
    slot->Owner = context;
    slot->SessionId = context->SessionId;
    slot->SidLength = context->SidLength;
    RtlCopyMemory(slot->Sid, context->Sid, context->SidLength);
    slot->Generation = message->Generation;
    slot->DistributionCount = message->DistributionCount;
    RtlCopyMemory(slot->Distributions, message->Distributions,
                  sizeof(message->Distributions));
  }
  ExReleasePushLockExclusive(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
  return status;
}

NTSTATUS
FswUnload(_In_ FLT_FILTER_UNLOAD_FLAGS Flags) {
  UNREFERENCED_PARAMETER(Flags);
  PAGED_CODE();
  FltCloseCommunicationPort(Globals.ServerPort);
  FltUnregisterFilter(Globals.Filter);
  return STATUS_SUCCESS;
}

NTSTATUS
DriverEntry(_In_ PDRIVER_OBJECT DriverObject,
            _In_ PUNICODE_STRING RegistryPath) {
  NTSTATUS status;
  PSECURITY_DESCRIPTOR securityDescriptor = NULL;
  OBJECT_ATTRIBUTES attributes;
  UNICODE_STRING portName = RTL_CONSTANT_STRING(FSW_FILTER_PORT_NAME);
  UNREFERENCED_PARAMETER(RegistryPath);

  RtlZeroMemory(&Globals, sizeof(Globals));
  ExInitializePushLock(&Globals.MappingsLock);
  status = FltRegisterFilter(DriverObject, &Registration, &Globals.Filter);
  if (!NT_SUCCESS(status)) {
    return status;
  }
  status = FswBuildPortSecurityDescriptor(&securityDescriptor);
  if (!NT_SUCCESS(status)) {
    FltUnregisterFilter(Globals.Filter);
    return status;
  }
  InitializeObjectAttributes(&attributes, &portName,
                             OBJ_KERNEL_HANDLE | OBJ_CASE_INSENSITIVE, NULL,
                             securityDescriptor);
  status = FltCreateCommunicationPort(
      Globals.Filter, &Globals.ServerPort, &attributes, NULL, FswPortConnect,
      FswPortDisconnect, FswPortMessage, FSW_MAX_INTERACTIVE_SESSIONS);
  ExFreePoolWithTag(securityDescriptor, FSW_POOL_TAG);
  if (!NT_SUCCESS(status)) {
    FltUnregisterFilter(Globals.Filter);
    return status;
  }
  status = FltStartFiltering(Globals.Filter);
  if (!NT_SUCCESS(status)) {
    FltCloseCommunicationPort(Globals.ServerPort);
    FltUnregisterFilter(Globals.Filter);
  }
  return status;
}
