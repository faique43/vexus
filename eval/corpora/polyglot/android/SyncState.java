package com.example.android;

/** How far the app's background upload has got. */
public enum SyncState {
    IDLE,
    UPLOADING_RECEIPTS,
    WAITING_FOR_WIFI,
    QUOTA_EXCEEDED
}
