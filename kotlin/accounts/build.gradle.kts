plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.serialization)
    `maven-publish`
    signing
}

// Equals the repo's shared release tag (`0.4.1`): one bare tag serves SwiftPM and JitPack. Bump it with
// `toddstackAccountsVersion` in swift/ when cutting a tag.
version = "0.4.1"

// JitPack (env JITPACK=true) serves only groups under `com.github.<user>.<repo>`; Maven Central will use me.toddgriffin.
val publishGroup: String =
    if (System.getenv("JITPACK") == "true") "com.github.goddtriffin.toddstack" else "me.toddgriffin"
group = publishGroup

android {
    namespace = "me.toddgriffin.toddstack.accounts"
    compileSdk = 36

    defaultConfig {
        minSdk = 26
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    testFixtures {
        enable = true
    }

    testOptions {
        unitTests.isReturnDefaultValues = true
    }

    lint {
        warningsAsErrors = true
        abortOnError = true
        // Versions are pinned on purpose (bumped by hand); core-ktx is not worth a dependency.
        disable += setOf("GradleDependency", "UseKtx")
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
            withJavadocJar()
        }
    }
}

dependencies {
    api(libs.coroutines.core)
    implementation(libs.serialization.json)
    implementation(libs.androidx.browser)
    implementation(libs.androidx.activity)
    implementation(libs.androidx.lifecycle.runtime)
    implementation(libs.androidx.datastore.preferences)

    testImplementation(libs.junit)
    testImplementation(libs.coroutines.test)
    testImplementation(testFixtures(project(":accounts")))
}

publishing {
    publications {
        register<MavenPublication>("release") {
            groupId = publishGroup
            artifactId = "toddstack-accounts"
            version = project.version.toString()
            afterEvaluate { from(components["release"]) }
            pom {
                name.set("toddstack-accounts")
                description.set("Accounts SDK for native Android apps: sign in through your project's server, never directly against accounts.toddgriffin.me.")
                url.set("https://github.com/goddtriffin/toddstack")
                licenses {
                    license {
                        name.set("MIT License")
                        url.set("https://opensource.org/licenses/MIT")
                    }
                }
                developers {
                    developer {
                        id.set("goddtriffin")
                        name.set("Todd Everett Griffin")
                        url.set("https://github.com/goddtriffin")
                    }
                }
                scm {
                    url.set("https://github.com/goddtriffin/toddstack")
                    connection.set("scm:git:https://github.com/goddtriffin/toddstack.git")
                    developerConnection.set("scm:git:ssh://git@github.com/goddtriffin/toddstack.git")
                }
            }
        }
    }
}

// Signs only when a key is supplied (ORG_GRADLE_PROJECT_signingInMemoryKey), so local builds need no credentials.
signing {
    val key: String? = providers.gradleProperty("signingInMemoryKey").orNull
    val password: String? = providers.gradleProperty("signingInMemoryKeyPassword").orNull
    if (key != null) {
        useInMemoryPgpKeys(key, password)
        sign(publishing.publications["release"])
    }
}
