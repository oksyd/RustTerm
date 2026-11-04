sftp-a-filename-is-not-valid-utf-8 = A filename is not valid UTF-8.

sftp-a-transfer-destination-changed-type-refresh-and-try-again = A transfer destination changed type. Refresh and try again.

sftp-a-transfer-source-changed-type-restart-the-transfer = A transfer source changed type. Restart the transfer.

sftp-additional-item-count =
    { $count ->
        [one] { "\u000A" }and { $count } more item
       *[other] { "\u000A" }and { $count } more items
    }

sftp-at-most-limit-files-can-be-scheduled-at-once-wait = At most { $limit } files can be scheduled at once; wait for current transfers and select the rest in batches.

sftp-cancel-transfers-discard-unuploaded-edits-and-close = Cancel transfers, discard unuploaded edits and close?

sftp-close-the-remote-file-editor = Close the remote file editor?

sftp-close-transfers-and-editor-warning = Active and queued transfers will be cancelled. Completed files will be kept. Close any external editor first.

sftp-close-transfers-warning = Active and queued transfers will be cancelled. Completed files will be kept.

sftp-could-not-create-folder-error = Could not create folder: { $error }

sftp-could-not-open-editor-error = Could not open editor: { $error }

sftp-could-not-open-the-desktop-text-editor = Could not open the desktop text editor.

sftp-could-not-preview-file-error = Could not preview file: { $error }

sftp-could-not-read-directory-error = Could not read directory: { $error }

sftp-could-not-rename-item-error = Could not rename item: { $error }

sftp-could-not-save-remote-file-error = Could not save remote file: { $error }

sftp-delete-selected-confirmation =
    { $count ->
        [one] Delete { $count } selected item?
       *[other] Delete { $count } selected items?
    }

sftp-directory-transfers-do-not-follow-symbolic-links-or-special-files = Directory transfers do not follow symbolic links or special files.

sftp-discard-edits = Discard edits

sftp-double-click-to-open-f4-to-preview-ctrl-shift-to = Double-click to open · F4 to preview · Ctrl/Shift to select

sftp-existing-files = Existing files:

sftp-external-editor = External editor

sftp-external-edits-must-be-a-regular-text-file = External edits must be a regular text file.

sftp-failed-error = Failed: { $error }

sftp-file-manager-dialog = SFTP file manager…

sftp-finish-editing-before-changing-folders = Finish editing before changing folders.

sftp-large-or-special-text-is-previewed-only-use-the-external = Large or special text is previewed only. Use the external editor to edit the complete file.

sftp-names-more-this-cannot-be-undone-only-files-and-empty =
    { $names }{ $more }

    This cannot be undone. Only files and empty folders are deleted; nonempty folders are not deleted recursively.

sftp-only-regular-files-can-be-downloaded = Only regular files can be downloaded

sftp-overwrite = Overwrite

sftp-overwrite-cannot-replace-folders-symbolic-links-or-special-files = Overwrite cannot replace folders, symbolic links, or special files.

sftp-preview-edit-text-f4 = Preview / edit text · F4

sftp-preview-line-count =
    { $count ->
        [one] { $count } line · Read only
       *[other] { $count } lines · Read only
    }

sftp-preview-supports-utf-8-text-files-up-to-1-mib = Preview supports UTF-8 text files up to 1 MiB.

sftp-remote-file-saved = Remote file saved.

sftp-remote-files-session = Remote files · { $session }

sftp-remote-text = Remote text

sftp-restart-transfer = Restart transfer

sftp-resume-interrupted-transfer = Resume interrupted transfer

sftp-save-in-the-external-editor-then-upload-your-edits-here = Save in the external editor, then upload your edits here.

sftp-select-at-most-limit-files-at-a-time-download-larger = Select at most { $limit } files at a time; download larger sets in batches.

sftp-select-at-most-limit-files-at-a-time-upload-larger = Select at most { $limit } files at a time; upload larger sets in batches.

sftp-select-files-or-folders-to-upload = Select files or folders to upload

sftp-selected-item-count =
    { $count ->
        [one] { $count } item · { $selected } selected
       *[other] { $count } items · { $selected } selected
    }

sftp-session-transfer-count =
    { $count ->
        [one] { $session } · { $count } transfer
       *[other] { $session } · { $count } transfers
    }

sftp-skip = Skip

sftp-some-items-could-not-be-deleted-errors =
    Some items could not be deleted:
    { $errors }

sftp-the-desktop-editor-did-not-respond-try-again = The desktop editor did not respond. Try again.

sftp-the-directory-tree-exceeds-the-transfer-limit-10-000-entries = The directory tree exceeds the transfer limit (10,000 entries or 64 levels).

sftp-the-partial-file-is-longer-than-its-source-restart-the = The partial file is longer than its source. Restart the transfer.

sftp-the-partial-upload-changed-on-the-server-restart-the-transfer = The partial upload changed on the server. Restart the transfer.

sftp-the-remote-file-changed-reload-it-before-uploading-your-edits = The remote file changed. Reload it before uploading your edits.

sftp-the-remote-file-changed-while-downloading-restart-the-transfer = The remote file changed while downloading. Restart the transfer.

sftp-the-server-returned-an-invalid-rename-response = The server returned an invalid rename response.

sftp-the-source-changed-or-cannot-be-verified-restart-the-transfer = The source changed or cannot be verified. Restart the transfer.

sftp-the-source-changed-since-the-interrupted-transfer-restart-the-transfer = The source changed since the interrupted transfer. Restart the transfer.

sftp-the-source-changed-while-it-was-being-uploaded-restart-the = The source changed while it was being uploaded. Restart the transfer.

sftp-the-transfer-contains-too-many-long-filenames = The transfer contains too many long filenames.

sftp-this-server-does-not-support-atomic-overwrite-choose-rename-instead = This server does not support atomic overwrite. Choose Rename instead.

sftp-too-many-conflicting-filenames-choose-another-destination = Too many conflicting filenames. Choose another destination.

sftp-transfer-item-count =
    { $count ->
        [one] Transfers · { $count } item
       *[other] Transfers · { $count } items
    }

sftp-unuploaded-edits-will-be-discarded-close-any-external-editor-first = Unuploaded edits will be discarded. Close any external editor first.

sftp-upload-edits = Upload edits
