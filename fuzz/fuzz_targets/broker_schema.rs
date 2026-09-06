#![no_main]

use libfuzzer_sys::fuzz_target;
use touchbar_broker_schema::*;

fuzz_target!(|bytes: &[u8]| {
    let _ = ContextReadRequest::decode(bytes);
    let _ = ContextSnapshot::decode(bytes);
    let _ = ContextSubscriptionOpened::decode(bytes);
    let _ = ClipboardReadRequest::decode(bytes);
    let _ = ClipboardWriteRequest::decode(bytes);
    let _ = ClipboardValue::decode(bytes);
    let _ = SecretReadRequest::decode(bytes);
    let _ = SecretValue::decode(bytes);
    let _ = LocalConnect::decode(bytes);
    let _ = LocalConnectionOpened::decode(bytes);
    let _ = LocalSendFrame::decode(bytes);
    let _ = LocalFrameEvent::decode(bytes);
    let _ = NotificationSend::decode(bytes);
    let _ = NotificationRemove::decode(bytes);
    let _ = UriOpenRequest::decode(bytes);
    let _ = FilesystemWriteFile::decode(bytes);
    let _ = FilesystemWriteStream::decode(bytes);
    let _ = FilesystemWriteStreamOpened::decode(bytes);
    let _ = FilesystemWriteStreamChunk::decode(bytes);
    let _ = FilesystemWriteStreamCommit::decode(bytes);
    let _ = FilesystemPath::decode(bytes);
    let _ = FilesystemRename::decode(bytes);
    let _ = FilesystemMutationResult::decode(bytes);
    let _ = CommandRunRequest::decode(bytes);
    let _ = CommandOpened::decode(bytes);
    let _ = CommandEvent::decode(bytes);
    let _ = HttpRequest::decode(bytes);
    let _ = HttpResponse::decode(bytes);
    let _ = HttpStreamOpened::decode(bytes);
    let _ = HttpStreamEvent::decode(bytes);
    let _ = FilesystemReadFile::decode(bytes);
    let _ = FilesystemReadStream::decode(bytes);
    let _ = FilesystemStreamOpened::decode(bytes);
    let _ = FilesystemStreamEvent::decode(bytes);
    let _ = FilesystemFileChunk::decode(bytes);
    let _ = FilesystemListDirectory::decode(bytes);
    let _ = FilesystemDirectoryEntries::decode(bytes);
    let _ = DbusCall::decode(bytes);
    let _ = DbusReply::decode(bytes);
    let _ = DbusSubscription::decode(bytes);
    let _ = DbusSubscriptionOpened::decode(bytes);
    let _ = DbusPropertiesChanged::decode(bytes);
});
