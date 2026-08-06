// 官方仓库优先（CI GitHub US runner 上 google()/mavenCentral() 稳定可靠；
// 阿里云镜像在 CI 上偶发超时，且 Gradle 遇 IO 错误会禁用该仓库导致连锁失败）。
// 阿里云镜像保留作 CN 网络兜底。
pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
        maven("https://maven.aliyun.com/repository/gradle-plugin")
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/central")
    }
}
dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
        maven("https://maven.aliyun.com/repository/google")
        maven("https://maven.aliyun.com/repository/central")
        maven("https://maven.aliyun.com/repository/public")
    }
}
rootProject.name = "AeroDesk"
include(":app")
