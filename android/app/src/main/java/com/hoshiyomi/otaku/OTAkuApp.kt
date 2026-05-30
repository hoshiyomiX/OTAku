package com.hoshiyomi.otaku

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build

class OTAkuApp : Application() {
    companion object {
        const val CHANNEL_ID = "otaku_service"
        const val CHANNEL_NAME = "OTAku Operations"
        const val CHANNEL_DESC = "Background OTAku processing notifications"
        lateinit var instance: OTAkuApp
            private set
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        createNotificationChannel()
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID,
                CHANNEL_NAME,
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = CHANNEL_DESC
            }
            val manager = getSystemService(NotificationManager::class.java)
            manager.createNotificationChannel(channel)
        }
    }
}
