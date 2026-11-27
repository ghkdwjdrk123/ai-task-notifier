plugins {
    id("com.android.application")
}

android {
    namespace = "io.github.ghkdwjdrk123.notifier"
    compileSdk = 36
    buildToolsVersion = "36.1.0"

    defaultConfig {
        applicationId = "io.github.ghkdwjdrk123.notifier"
        minSdk = 31
        targetSdk = 36
        versionCode = 14
        versionName = "0.3.1"
    }

    // 정식 서명: 키 경로·비밀번호는 환경 변수로만 받는다(저장소에 두지 않음).
    // android/publish.sh가 맥 키체인에서 비밀번호를 읽어 넘긴다. 없으면 서명하지 않은 release가 나온다.
    val releaseKeystore = System.getenv("NOTIFIER_KEYSTORE")
    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storeType = "PKCS12"
                storePassword = System.getenv("NOTIFIER_KEYSTORE_PASSWORD")
                keyAlias = "notifier"
                keyPassword = System.getenv("NOTIFIER_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            if (releaseKeystore != null) signingConfig = signingConfigs.getByName("release")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
