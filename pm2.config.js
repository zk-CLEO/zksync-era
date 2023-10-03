module.exports = {
    apps: [{
        name: "zksync-server",
        script: "zk server",
        // script: "node -- /home/developer/zksync-era/infrastructure/zk/build/index.js",
        // args: "server",
        cwd: "/home/developer/zksync-era/bin",
        env: {
            ZKSYNC_HOME: "/home/developer/zksync-era",
        },
        // watch: true,
        error_file: "/home/developer/zksync-era/logs/zksync-era/zksync-era-error.log"
    }]
}
