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

//
//  Session and user identity of the requestor, captured from its primary
//  token in a single pass.  The SID array follows two ULONGs, so it is
//  4-byte aligned as SID requires; do not reorder the fields.
//
typedef struct _FSW_REQUESTOR_IDENTITY {
  ULONG SessionId;
  ULONG SidLength;
  UCHAR Sid[SECURITY_MAX_SID_SIZE];
} FSW_REQUESTOR_IDENTITY, *PFSW_REQUESTOR_IDENTITY;

typedef struct _FSW_GLOBALS {
  PFLT_FILTER Filter;
  PFLT_PORT ServerPort;
  EX_PUSH_LOCK MappingsLock;
  FSW_SESSION_MAPPINGS Mappings[FSW_MAX_INTERACTIVE_SESSIONS];
} FSW_GLOBALS;

FSW_GLOBALS Globals;

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

_Must_inspect_result_
_IRQL_requires_max_(PASSIVE_LEVEL)
static NTSTATUS FswQueryRequestorIdentity(
    _In_ PEPROCESS Process,
    _Out_ PFSW_REQUESTOR_IDENTITY Identity,
    _Out_ PBOOLEAN Eligible);
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN FswIsValidDistributionName(
    _In_reads_(FSW_MAX_DISTRIBUTION_NAME) const WCHAR* Name);
_IRQL_requires_max_(APC_LEVEL)
static VOID FswClearMappingsForOwner(_In_ PFSW_CONNECTION_CONTEXT Owner);
_Must_inspect_result_
_IRQL_requires_max_(PASSIVE_LEVEL)
static NTSTATUS FswBuildPortSecurityDescriptor(
    _Outptr_result_maybenull_ PSECURITY_DESCRIPTOR* Descriptor);
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN FswSplitFirstComponent(_In_ PCUNICODE_STRING RelativeName,
                                      _Out_ PUNICODE_STRING FirstComponent,
                                      _Out_ PUNICODE_STRING Remainder);
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN FswIsCandidateDistribution(
    _In_ PCUNICODE_STRING FirstComponent);
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN FswOwnsDistribution(_In_ PFSW_REQUESTOR_IDENTITY Identity,
                                   _In_ PCUNICODE_STRING FirstComponent);
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static NTSTATUS FswBuildTargetName(_In_ PCUNICODE_STRING Distribution,
                                   _In_ PCUNICODE_STRING Remainder,
                                   _Out_ PUNICODE_STRING TargetName);

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
#pragma alloc_text(PAGE, FswQueryRequestorIdentity)
#pragma alloc_text(PAGE, FswIsValidDistributionName)
#pragma alloc_text(PAGE, FswClearMappingsForOwner)
#pragma alloc_text(PAGE, FswBuildPortSecurityDescriptor)
#pragma alloc_text(PAGE, FswSplitFirstComponent)
#pragma alloc_text(PAGE, FswIsCandidateDistribution)
#pragma alloc_text(PAGE, FswOwnsDistribution)
#pragma alloc_text(PAGE, FswBuildTargetName)
#endif

//
//  Reads session id, user SID, integrity level and AppContainer state from the
//  requestor's primary token in one reference.  Splitting this over two
//  functions cost five token queries (and five pool allocations) on every
//  create; this costs four, and only on a name that could actually match.
//
//  The returned status covers the identity only.  A token whose integrity or
//  AppContainer state cannot be read is reported as *not* eligible while the
//  identity still succeeds, because FswPortConnect wants the identity and
//  does not care about eligibility.
//
_Must_inspect_result_
_IRQL_requires_max_(PASSIVE_LEVEL)
static NTSTATUS
FswQueryRequestorIdentity(_In_ PEPROCESS Process,
                          _Out_ PFSW_REQUESTOR_IDENTITY Identity,
                          _Out_ PBOOLEAN Eligible) {
  PACCESS_TOKEN token;
  PVOID userInformation = NULL;
  PVOID integrityInformation = NULL;
  PVOID appContainerInformation = NULL;
  PTOKEN_USER tokenUser;
  PTOKEN_MANDATORY_LABEL label;
  ULONG sessionId = 0;
  ULONG integrityLevel = 0;
  ULONG sidLength;
  UCHAR subAuthorityCount;
  NTSTATUS status;

  PAGED_CODE();
  RtlZeroMemory(Identity, sizeof(*Identity));
  *Eligible = FALSE;

  token = PsReferencePrimaryToken(Process);
  //  SeQuerySessionIdToken writes the session id by value, so there is no
  //  out-buffer to allocate, free, or null-check.  The older
  //  SeQueryInformationToken(token, TokenSessionId, ...) path returned
  //  STATUS_SUCCESS yet left the out-pointer NULL on Windows 11 ARM64, so the
  //  *(PULONG) read faulted at address 0 (issue #36).
  status = SeQuerySessionIdToken(token, &sessionId);
  if (NT_SUCCESS(status)) {
    status = SeQueryInformationToken(token, TokenUser, &userInformation);
  }
  if (NT_SUCCESS(status)) {
    if (userInformation == NULL) {
      status = STATUS_INVALID_SID;
    } else {
      tokenUser = (PTOKEN_USER)userInformation;
      if (!RtlValidSid(tokenUser->User.Sid)) {
        status = STATUS_INVALID_SID;
      } else {
        sidLength = RtlLengthSid(tokenUser->User.Sid);
        if (sidLength > SECURITY_MAX_SID_SIZE) {
          status = STATUS_INVALID_SID;
        } else {
          Identity->SessionId = sessionId;
          Identity->SidLength = sidLength;
          status = RtlCopySid(SECURITY_MAX_SID_SIZE, (PSID)Identity->Sid,
                              tokenUser->User.Sid);
        }
      }
    }
  }
  if (NT_SUCCESS(status) &&
      NT_SUCCESS(SeQueryInformationToken(token, TokenIntegrityLevel,
                                         &integrityInformation)) &&
      integrityInformation != NULL &&
      NT_SUCCESS(SeQueryInformationToken(token, TokenIsAppContainer,
                                         &appContainerInformation)) &&
      appContainerInformation != NULL) {
    label = (PTOKEN_MANDATORY_LABEL)integrityInformation;
    if (RtlValidSid(label->Label.Sid)) {
      subAuthorityCount = *RtlSubAuthorityCountSid(label->Label.Sid);
      if (subAuthorityCount != 0) {
        integrityLevel = *RtlSubAuthoritySid(label->Label.Sid,
                                             (ULONG)(subAuthorityCount - 1));
      }
    }
    if (integrityLevel >= SECURITY_MANDATORY_MEDIUM_RID &&
        *(PULONG)appContainerInformation == 0 && Identity->SessionId != 0) {
      *Eligible = TRUE;
    }
  }
  if (appContainerInformation != NULL) {
    ExFreePool(appContainerInformation);
  }
  if (integrityInformation != NULL) {
    ExFreePool(integrityInformation);
  }
  if (userInformation != NULL) {
    ExFreePool(userInformation);
  }
  PsDereferencePrimaryToken(token);
  return status;
}

_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN
FswIsValidDistributionName(
    _In_reads_(FSW_MAX_DISTRIBUTION_NAME) const WCHAR* Name) {
  ULONG length = 0;

  PAGED_CODE();
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

_IRQL_requires_max_(APC_LEVEL)
static VOID
FswClearMappingsForOwner(_In_ PFSW_CONNECTION_CONTEXT Owner) {
  PAGED_CODE();
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

_Must_inspect_result_
_IRQL_requires_max_(PASSIVE_LEVEL)
static NTSTATUS
FswBuildPortSecurityDescriptor(
    _Outptr_result_maybenull_ PSECURITY_DESCRIPTOR* Descriptor) {
  const ULONG aclSize = sizeof(ACL) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeLocalSystemSid)) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeAliasAdminsSid)) +
      (sizeof(ACCESS_ALLOWED_ACE) - sizeof(ULONG) +
       RtlLengthSid(SeExports->SeInteractiveSid));
  const ULONG totalSize = SECURITY_DESCRIPTOR_MIN_LENGTH + aclSize;
  PUCHAR memory;
  PACL acl;
  NTSTATUS status;

  PAGED_CODE();
  *Descriptor = NULL;
  memory = ExAllocatePool2(POOL_FLAG_PAGED, totalSize, FSW_POOL_TAG);
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

//
//  Splits a volume-relative name such as `\Ubuntu\etc\hosts` into its first
//  component (`Ubuntu`) and the remainder (`\etc\hosts`).  Purely textual: it
//  allocates nothing and takes no lock, and both outputs alias the caller's
//  buffer, so they are valid only while the name information is held.
//
//  Returns FALSE for an empty first component and for one longer than a
//  distribution name can be, which is the bound that keeps the comparison
//  below from ever looking at an over-long segment.  A name whose first
//  component carries a stream separator (`Ubuntu:zone`) is returned intact and
//  simply fails the comparison: FswIsValidDistributionName rejects `:` in a
//  registered name, so no stored name can ever match one.
//
//  A remainder made only of separators (`C:\Ubuntu\`) is flattened to empty so
//  the distribution root builds as `\??\UNC\wsl.localhost\Ubuntu` with no
//  trailing separator.
//
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN
FswSplitFirstComponent(_In_ PCUNICODE_STRING RelativeName,
                       _Out_ PUNICODE_STRING FirstComponent,
                       _Out_ PUNICODE_STRING Remainder) {
  UNICODE_STRING body = *RelativeName;
  USHORT characterCount;
  USHORT index;

  PAGED_CODE();
  RtlZeroMemory(FirstComponent, sizeof(*FirstComponent));
  RtlZeroMemory(Remainder, sizeof(*Remainder));

  while (body.Length >= sizeof(WCHAR) && body.Buffer != NULL &&
         body.Buffer[0] == L'\\') {
    body.Buffer += 1;
    body.Length -= sizeof(WCHAR);
    body.MaximumLength = body.Length;
  }
  if (body.Length == 0 || body.Buffer == NULL) {
    return FALSE;
  }
  characterCount = (USHORT)(body.Length / sizeof(WCHAR));
  for (index = 0; index < characterCount; ++index) {
    if (body.Buffer[index] == L'\\') {
      break;
    }
  }
  if (index == 0 || (ULONG)index > (FSW_MAX_DISTRIBUTION_NAME - 1u)) {
    return FALSE;
  }
  FirstComponent->Buffer = body.Buffer;
  FirstComponent->Length = (USHORT)(index * sizeof(WCHAR));
  FirstComponent->MaximumLength = FirstComponent->Length;

  Remainder->Buffer = body.Buffer + index;
  Remainder->Length = (USHORT)(body.Length - FirstComponent->Length);
  Remainder->MaximumLength = Remainder->Length;
  characterCount = (USHORT)(Remainder->Length / sizeof(WCHAR));
  for (index = 0; index < characterCount; ++index) {
    if (Remainder->Buffer[index] != L'\\') {
      return TRUE;
    }
  }
  Remainder->Length = 0;
  Remainder->MaximumLength = 0;
  return TRUE;
}

//
//  Cheap gate on the create path: does this component match a distribution in
//  any occupied slot, regardless of owner?  One RtlEqualUnicodeString per
//  registered name under the shared lock, no allocation and no token work.
//  With no broker connected every slot is empty and this is a bounded scan of
//  16 NULL owners.  A hit is only a candidate; FswOwnsDistribution then does
//  the real per-(session, SID) lookup.
//
_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN
FswIsCandidateDistribution(_In_ PCUNICODE_STRING FirstComponent) {
  BOOLEAN candidate = FALSE;

  PAGED_CODE();
  KeEnterCriticalRegion();
  ExAcquirePushLockShared(&Globals.MappingsLock);
  for (ULONG slotIndex = 0;
       !candidate && slotIndex < FSW_MAX_INTERACTIVE_SESSIONS; ++slotIndex) {
    PFSW_SESSION_MAPPINGS slot = &Globals.Mappings[slotIndex];
    if (slot->Owner == NULL) {
      continue;
    }
    for (ULONG index = 0; index < slot->DistributionCount; ++index) {
      UNICODE_STRING name;
      RtlInitUnicodeString(&name, slot->Distributions[index]);
      if (RtlEqualUnicodeString(&name, FirstComponent, TRUE)) {
        candidate = TRUE;
        break;
      }
    }
  }
  ExReleasePushLockShared(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
  return candidate;
}

_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static BOOLEAN
FswOwnsDistribution(_In_ PFSW_REQUESTOR_IDENTITY Identity,
                    _In_ PCUNICODE_STRING FirstComponent) {
  BOOLEAN found = FALSE;

  PAGED_CODE();
  KeEnterCriticalRegion();
  ExAcquirePushLockShared(&Globals.MappingsLock);
  for (ULONG slotIndex = 0; slotIndex < FSW_MAX_INTERACTIVE_SESSIONS;
       ++slotIndex) {
    PFSW_SESSION_MAPPINGS slot = &Globals.Mappings[slotIndex];
    if (slot->Owner == NULL || slot->SessionId != Identity->SessionId ||
        !RtlEqualSid((PSID)slot->Sid, (PSID)Identity->Sid)) {
      continue;
    }
    for (ULONG index = 0; index < slot->DistributionCount; ++index) {
      UNICODE_STRING name;
      RtlInitUnicodeString(&name, slot->Distributions[index]);
      if (RtlEqualUnicodeString(&name, FirstComponent, TRUE)) {
        found = TRUE;
        break;
      }
    }
    break;
  }
  ExReleasePushLockShared(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
  return found;
}

_Must_inspect_result_
_IRQL_requires_max_(APC_LEVEL)
static NTSTATUS
FswBuildTargetName(_In_ PCUNICODE_STRING Distribution,
                   _In_ PCUNICODE_STRING Remainder,
                   _Out_ PUNICODE_STRING TargetName) {
  const UNICODE_STRING prefix =
      RTL_CONSTANT_STRING(L"\\??\\UNC\\wsl.localhost\\");
  const ULONG required = prefix.Length + Distribution->Length +
                         Remainder->Length + sizeof(WCHAR);

  PAGED_CODE();
  RtlZeroMemory(TargetName, sizeof(*TargetName));
  if (required > MAXUSHORT) {
    return STATUS_NAME_TOO_LONG;
  }
  TargetName->Buffer = ExAllocatePool2(POOL_FLAG_PAGED, required,
                                       FSW_POOL_TAG);
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

//
//  Fail-open review.  Redirection is a convenience; a filter that fails a
//  create is a filter that breaks the machine.  Every exit below other than
//  the final one leaves the create exactly as the caller issued it, and
//  releases everything it took.  The complete list of fail-open points, in
//  the order they are reached:
//
//    1. not an IRP-based create, kernel-mode requestor, or IRQL above
//       PASSIVE_LEVEL                                    -> pass through
//    2. paging-file open, volume open, open-by-file-id, or a target-directory
//       open (SL_OPEN_TARGET_DIRECTORY - the IRP-level spelling of the
//       rename/link "FILE_OPEN_TARGET_DIRECTORY" create; the caller wants the
//       parent directory back, and reparsing it would retarget the rename)
//                                                        -> pass through
//    3. relative open (RelatedFileObject != NULL).  STATUS_REPARSE hands the
//       object manager an absolute `\??\UNC\...` name, but a relative open
//       re-parses it against the related object, which yields a nonsense path.
//       Win32 CreateFileW never issues one; only NtCreateFile with
//       RootDirectory does                               -> pass through
//    4. FltGetFileNameInformation failure (name not available in pre-create,
//       for instance during a low-resource or reentrant open)
//                                                        -> pass through
//    5. FltParseFileNameInformation failure, or a name with no volume-relative
//       part                              -> release name info, pass through
//    6. an empty or over-long first component
//                                         -> release name info, pass through
//    7. no distribution in any slot matches the first component
//                                         -> release name info, pass through
//    8. requestor process unavailable, token query failure, or a requestor
//       that is not eligible (integrity below medium, AppContainer,
//       session 0)                        -> release name info, pass through
//    9. no mapping for this exact (session, SID)
//                                         -> release name info, pass through
//   10. target-name allocation failure, name too long, or
//       IoReplaceFileObjectName failure   -> release everything, pass through
//
//  There are no assertions here on purpose: an assertion is a bugcheck on a
//  checked build, and none of the conditions above is a driver bug.  Nothing
//  on this path logs, and no path text ever leaves kernel memory.
//
FLT_PREOP_CALLBACK_STATUS
FswPreCreate(_Inout_ PFLT_CALLBACK_DATA Data,
             _In_ PCFLT_RELATED_OBJECTS FltObjects,
             _Flt_CompletionContext_Outptr_ PVOID* CompletionContext) {
  PFLT_FILE_NAME_INFORMATION nameInfo = NULL;
  UNICODE_STRING relativeName;
  UNICODE_STRING firstComponent;
  UNICODE_STRING remainder;
  UNICODE_STRING targetName;
  FSW_REQUESTOR_IDENTITY identity;
  BOOLEAN eligible = FALSE;
  PEPROCESS process;
  NTSTATUS status;

  UNREFERENCED_PARAMETER(CompletionContext);
  PAGED_CODE();

  //
  //  Stage 1 - constant-cost rejects.  Nothing here allocates or takes a
  //  lock, so the overwhelming majority of creates on the machine leave the
  //  filter within a handful of instructions.
  //
  if (!FLT_IS_IRP_OPERATION(Data) || Data->RequestorMode != UserMode ||
      KeGetCurrentIrql() != PASSIVE_LEVEL ||
      FlagOn(Data->Iopb->OperationFlags,
             SL_OPEN_PAGING_FILE | SL_OPEN_TARGET_DIRECTORY) ||
      FlagOn(Data->Iopb->TargetFileObject->Flags, FO_VOLUME_OPEN) ||
      FlagOn(Data->Iopb->Parameters.Create.Options, FILE_OPEN_BY_FILE_ID)) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  if (FltObjects->FileObject == NULL ||
      FltObjects->FileObject->RelatedFileObject != NULL) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }

  //
  //  Stage 2 - the name.  This runs before any token work: a token reference
  //  plus four SeQueryInformationToken allocations on every create was the
  //  filter's whole cost on paths it never touches.
  //
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
  if (!FswSplitFirstComponent(&relativeName, &firstComponent, &remainder) ||
      !FswIsCandidateDistribution(&firstComponent)) {
    FltReleaseFileNameInformation(nameInfo);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }

  //
  //  Stage 3 - identity.  Only a name that could belong to some session's
  //  mapping pays for this, and it is a single token reference.
  //
  process = FltGetRequestorProcess(Data);
  if (process == NULL ||
      !NT_SUCCESS(FswQueryRequestorIdentity(process, &identity, &eligible)) ||
      !eligible || !FswOwnsDistribution(&identity, &firstComponent)) {
    FltReleaseFileNameInformation(nameInfo);
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }

  //
  //  Stage 4 - rewrite.  An empty remainder is the distribution root and
  //  produces `\??\UNC\wsl.localhost\<distro>` with no trailing separator.
  //
  status = FswBuildTargetName(&firstComponent, &remainder, &targetName);
  if (NT_SUCCESS(status)) {
    status = IoReplaceFileObjectName(FltObjects->FileObject,
                                     targetName.Buffer, targetName.Length);
    ExFreePoolWithTag(targetName.Buffer, FSW_POOL_TAG);
  }
  FltReleaseFileNameInformation(nameInfo);
  if (!NT_SUCCESS(status)) {
    return FLT_PREOP_SUCCESS_NO_CALLBACK;
  }
  Data->IoStatus.Status = STATUS_REPARSE;
  Data->IoStatus.Information = IO_REPARSE;
  FltSetCallbackDataDirty(Data);
  return FLT_PREOP_COMPLETE;
}

NTSTATUS
FswPortConnect(_In_ PFLT_PORT ClientPort,
               _In_opt_ PVOID ServerPortCookie,
               _In_reads_bytes_opt_(SizeOfContext) PVOID ConnectionContext,
               _In_ ULONG SizeOfContext,
               _Outptr_result_maybenull_ PVOID* ConnectionPortCookie) {
  PFSW_CONNECTION_CONTEXT context;
  FSW_REQUESTOR_IDENTITY identity;
  BOOLEAN eligible = FALSE;
  NTSTATUS status;
  UNREFERENCED_PARAMETER(ServerPortCookie);
  UNREFERENCED_PARAMETER(ConnectionContext);
  UNREFERENCED_PARAMETER(SizeOfContext);
  PAGED_CODE();

  *ConnectionPortCookie = NULL;
  context = ExAllocatePool2(POOL_FLAG_PAGED, sizeof(*context), FSW_POOL_TAG);
  if (context == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  RtlZeroMemory(context, sizeof(*context));
  context->ClientPort = ClientPort;
  //
  //  Eligibility is deliberately ignored here: the port DACL already limits
  //  connections to SYSTEM, administrators and interactive users, and a
  //  broker's own integrity level is not what decides whether a *create* is
  //  redirected.  Only the identity matters, and session 0 never gets a slot.
  //
  status = FswQueryRequestorIdentity(PsGetCurrentProcess(), &identity,
                                     &eligible);
  if (!NT_SUCCESS(status) || identity.SessionId == 0) {
    ExFreePoolWithTag(context, FSW_POOL_TAG);
    return NT_SUCCESS(status) ? STATUS_ACCESS_DENIED : status;
  }
  context->SessionId = identity.SessionId;
  context->SidLength = identity.SidLength;
  RtlCopyMemory(context->Sid, identity.Sid, identity.SidLength);
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

//
//  Port message handler.
//
//  InputBuffer and OutputBuffer are raw user-mode pointers owned by the
//  caller for the duration of FilterSendMessage.  The message is therefore
//  probed and copied into pool once, and every check below reads the copy:
//  validating DistributionCount in user memory and then reading it again to
//  bound a copy is a double fetch the caller controls.  The capture also
//  keeps a paged-out user page from faulting while the push lock is held.
//
//  Ping replies with the driver's protocol version when the caller supplies at
//  least a ULONG of output buffer, and succeeds either way; a caller that
//  passes no output buffer (every caller today) sees exactly the old
//  behaviour.
//
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
  PFSW_SESSION_MAPPINGS ownSlot = NULL;
  PFSW_SESSION_MAPPINGS identitySlot = NULL;
  PFSW_SESSION_MAPPINGS freeSlot = NULL;
  BOOLEAN takeover = FALSE;
  NTSTATUS status = STATUS_SUCCESS;
  PAGED_CODE();

  *ReturnOutputBufferLength = 0;
  if (context == NULL || InputBuffer == NULL ||
      InputBufferLength != sizeof(FSW_MAPPING_MESSAGE)) {
    return STATUS_INFO_LENGTH_MISMATCH;
  }
  message = ExAllocatePool2(POOL_FLAG_PAGED, sizeof(FSW_MAPPING_MESSAGE),
                            FSW_POOL_TAG);
  if (message == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }
  __try {
    ProbeForRead(InputBuffer, sizeof(FSW_MAPPING_MESSAGE), sizeof(UCHAR));
    RtlCopyMemory(message, InputBuffer, sizeof(FSW_MAPPING_MESSAGE));
  } __except (EXCEPTION_EXECUTE_HANDLER) {
    ExFreePoolWithTag(message, FSW_POOL_TAG);
    return STATUS_INVALID_USER_BUFFER;
  }

  if (message->Version != FSW_PROTOCOL_VERSION ||
      message->Size != sizeof(FSW_MAPPING_MESSAGE) || message->Reserved != 0 ||
      message->DistributionCount > FSW_MAX_DISTRIBUTIONS) {
    ExFreePoolWithTag(message, FSW_POOL_TAG);
    return STATUS_INVALID_PARAMETER;
  }
  if (message->Operation == FswOperationPing) {
    if (OutputBuffer != NULL && OutputBufferLength >= sizeof(ULONG)) {
      const ULONG protocolVersion = FSW_PROTOCOL_VERSION;
      __try {
        //
        //  Byte alignment, and a copy rather than a store: the caller owns
        //  this buffer and nothing requires it to be ULONG-aligned.
        //
        ProbeForWrite(OutputBuffer, sizeof(ULONG), sizeof(UCHAR));
        RtlCopyMemory(OutputBuffer, &protocolVersion, sizeof(ULONG));
        *ReturnOutputBufferLength = sizeof(ULONG);
      } __except (EXCEPTION_EXECUTE_HANDLER) {
        *ReturnOutputBufferLength = 0;
      }
    }
    ExFreePoolWithTag(message, FSW_POOL_TAG);
    return STATUS_SUCCESS;
  }
  if (message->Operation != FswOperationReplaceMappings &&
      message->Operation != FswOperationClearMappings) {
    ExFreePoolWithTag(message, FSW_POOL_TAG);
    return STATUS_INVALID_PARAMETER;
  }
  for (ULONG index = 0; index < message->DistributionCount; ++index) {
    if (!FswIsValidDistributionName(message->Distributions[index])) {
      ExFreePoolWithTag(message, FSW_POOL_TAG);
      return STATUS_INVALID_PARAMETER;
    }
  }

  KeEnterCriticalRegion();
  ExAcquirePushLockExclusive(&Globals.MappingsLock);
  //
  //  One slot per (session, SID).  A broker that crashed leaves its slot owned
  //  by a connection whose disconnect callback has not run yet; the
  //  replacement broker connects with the same identity and must take that
  //  slot over rather than consume a second one, or sixteen crashes would
  //  exhaust the table.
  //
  for (ULONG index = 0; index < FSW_MAX_INTERACTIVE_SESSIONS; ++index) {
    PFSW_SESSION_MAPPINGS candidate = &Globals.Mappings[index];
    if (candidate->Owner == context) {
      ownSlot = candidate;
      break;
    }
    if (candidate->Owner == NULL) {
      if (freeSlot == NULL) {
        freeSlot = candidate;
      }
    } else if (identitySlot == NULL &&
               candidate->SessionId == context->SessionId &&
               RtlEqualSid((PSID)candidate->Sid, (PSID)context->Sid)) {
      identitySlot = candidate;
    }
  }
  if (ownSlot != NULL) {
    slot = ownSlot;
  } else if (identitySlot != NULL) {
    slot = identitySlot;
    takeover = TRUE;
  } else {
    slot = freeSlot;
  }
  if (slot == NULL) {
    status = STATUS_INSUFFICIENT_RESOURCES;
  } else if (!takeover && ownSlot != NULL &&
             message->Generation < slot->Generation) {
    //
    //  Monotonic only against the same owner.  A new broker process restarts
    //  its GetTickCount64 generation from the same clock, so a takeover
    //  legitimately resets it and only a regression from the identical
    //  connection is a stale message.
    //
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
    //
    //  Only the validated prefix is stored, so every name the create path can
    //  read is known to be NUL-terminated inside its array.
    //
    RtlCopyMemory(slot->Distributions, message->Distributions,
                  message->DistributionCount *
                      sizeof(message->Distributions[0]));
  }
  ExReleasePushLockExclusive(&Globals.MappingsLock);
  KeLeaveCriticalRegion();
  ExFreePoolWithTag(message, FSW_POOL_TAG);
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
